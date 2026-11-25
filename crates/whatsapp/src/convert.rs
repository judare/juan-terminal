//! `wa::Message` (protobuf) → flat rows for the cache.

use whatsapp_rust::prelude::*;
use whatsapp_rust::waproto::buffa::Message as _;

use crate::model::MsgKind;

/// What a protobuf message means for the cache.
pub enum Parsed {
    /// A message to show.
    Message(Content),
    /// Reaction to `target` (empty emoji = removed).
    Reaction { target: String, emoji: String },
    /// Deleted for everyone.
    Revoke { target: String },
    /// Text edited.
    Edit { target: String, text: String },
    /// Protocol / system noise we don't show.
    Ignore,
}

#[derive(Default)]
pub struct Content {
    pub kind: Option<MsgKind>,
    pub text: String,
    pub mime: Option<String>,
    pub file_name: Option<String>,
    pub duration: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub quoted_id: Option<String>,
    pub quoted_sender: Option<String>,
    pub quoted_text: Option<String>,
    pub quoted_kind: Option<MsgKind>,
    /// Serialized base message (kept for media download / quoting / forwarding stickers).
    pub proto: Option<Vec<u8>>,
}

fn quote_from(ci: Option<&wa::ContextInfo>, c: &mut Content) {
    let Some(ci) = ci else { return };
    let Some(id) = ci.stanza_id.clone().filter(|s| !s.is_empty()) else { return };
    c.quoted_id = Some(id);
    c.quoted_sender = ci.participant.clone();
    if let Some(q) = ci.quoted_message.as_option() {
        let (kind, text) = summary(q);
        c.quoted_kind = Some(kind);
        c.quoted_text = Some(text);
    }
}

/// Kind and text of a message (for quotes and previews).
pub fn summary(msg: &wa::Message) -> (MsgKind, String) {
    match parse(msg, false) {
        Parsed::Message(c) => (c.kind.unwrap_or(MsgKind::Other), c.text),
        _ => (MsgKind::Other, String::new()),
    }
}

pub fn parse(msg: &wa::Message, keep_proto: bool) -> Parsed {
    let m = msg.get_base_message();
    if let Some(p) = m.protocol_message.as_option() {
        use wa::message::protocol_message::Type;
        let target = p.key.as_option().and_then(|k| k.id.clone()).unwrap_or_default();
        return match p.r#type {
            Some(Type::REVOKE) if !target.is_empty() => Parsed::Revoke { target },
            Some(Type::MESSAGE_EDIT) if !target.is_empty() => {
                let text = p.edited_message.as_option().map(|e| summary(e).1).unwrap_or_default();
                Parsed::Edit { target, text }
            }
            _ => Parsed::Ignore,
        };
    }
    if let Some(r) = m.reaction_message.as_option() {
        let target = r.key.as_option().and_then(|k| k.id.clone()).unwrap_or_default();
        if target.is_empty() {
            return Parsed::Ignore;
        }
        return Parsed::Reaction { target, emoji: r.text.clone().unwrap_or_default() };
    }
    let mut c = Content::default();
    if let Some(t) = m.conversation.as_deref().filter(|t| !t.is_empty()) {
        c.kind = Some(MsgKind::Text);
        c.text = t.to_string();
    } else if let Some(e) = m.extended_text_message.as_option() {
        c.kind = Some(MsgKind::Text);
        c.text = e.text.clone().unwrap_or_default();
        quote_from(e.context_info.as_option(), &mut c);
    } else if let Some(i) = m.image_message.as_option() {
        c.kind = Some(MsgKind::Image);
        c.text = i.caption.clone().unwrap_or_default();
        c.mime = i.mimetype.clone();
        c.width = i.width;
        c.height = i.height;
        quote_from(i.context_info.as_option(), &mut c);
    } else if let Some(v) = m.video_message.as_option() {
        c.kind = Some(MsgKind::Video);
        c.text = v.caption.clone().unwrap_or_default();
        c.mime = v.mimetype.clone();
        c.duration = v.seconds;
        c.width = v.width;
        c.height = v.height;
        quote_from(v.context_info.as_option(), &mut c);
    } else if let Some(a) = m.audio_message.as_option() {
        c.kind = Some(if a.ptt.unwrap_or(false) { MsgKind::Voice } else { MsgKind::Audio });
        c.mime = a.mimetype.clone();
        c.duration = a.seconds;
        quote_from(a.context_info.as_option(), &mut c);
    } else if let Some(s) = m.sticker_message.as_option() {
        c.kind = Some(MsgKind::Sticker);
        c.mime = s.mimetype.clone();
        c.width = s.width;
        c.height = s.height;
        quote_from(s.context_info.as_option(), &mut c);
    } else if let Some(d) = m.document_message.as_option() {
        c.kind = Some(MsgKind::Document);
        c.file_name = d.file_name.clone().or_else(|| d.title.clone());
        c.text = d.caption.clone().filter(|s| !s.is_empty()).or_else(|| c.file_name.clone()).unwrap_or_default();
        c.mime = d.mimetype.clone();
        quote_from(d.context_info.as_option(), &mut c);
    } else if let Some(l) = m.location_message.as_option() {
        c.kind = Some(MsgKind::Location);
        let (lat, lon) = (l.degrees_latitude.unwrap_or(0.), l.degrees_longitude.unwrap_or(0.));
        let place = [l.name.clone(), l.address.clone()].into_iter().flatten().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", ");
        c.text = format!("{place}{}https://maps.google.com/?q={lat},{lon}", if place.is_empty() { "" } else { "\n" });
    } else if let Some(l) = m.live_location_message.as_option() {
        c.kind = Some(MsgKind::Location);
        c.text = format!("Ubicación en tiempo real\nhttps://maps.google.com/?q={},{}", l.degrees_latitude.unwrap_or(0.), l.degrees_longitude.unwrap_or(0.));
    } else if let Some(ct) = m.contact_message.as_option() {
        c.kind = Some(MsgKind::Contact);
        let phone = ct.vcard.as_deref().and_then(vcard_phone).unwrap_or_default();
        c.text = format!("{} {phone}", ct.display_name.clone().unwrap_or_default()).trim().to_string();
    } else if let Some(ca) = m.contacts_array_message.as_option() {
        c.kind = Some(MsgKind::Contact);
        c.text = ca.contacts.iter().filter_map(|x| x.display_name.clone()).collect::<Vec<_>>().join(", ");
    } else if let Some(p) = m.poll_creation_message.as_option().or(m.poll_creation_message_v2.as_option()).or(m.poll_creation_message_v3.as_option()) {
        c.kind = Some(MsgKind::Poll);
        let opts = p.options.iter().filter_map(|o| o.option_name.clone()).map(|o| format!("• {o}")).collect::<Vec<_>>().join("\n");
        c.text = format!("{}\n{opts}", p.name.clone().unwrap_or_default());
    } else if m.poll_update_message.is_set()
        || m.sender_key_distribution_message.is_set()
        || m.keep_in_chat_message.is_set()
        || m.pin_in_chat_message.is_set()
        || m.enc_reaction_message.is_set()
        || m.message_history_notice.is_set()
    {
        return Parsed::Ignore;
    } else {
        c.kind = Some(MsgKind::Other);
    }
    if keep_proto && c.kind.is_some_and(|k| k.has_media()) {
        c.proto = Some(m.encode_to_vec());
    }
    Parsed::Message(c)
}

fn vcard_phone(v: &str) -> Option<String> {
    v.lines().find(|l| l.starts_with("TEL")).and_then(|l| l.split(':').next_back()).map(|s| s.trim().to_string())
}

/// Decode a stored base message.
pub fn decode(bytes: &[u8]) -> Option<wa::Message> {
    wa::Message::decode_from_slice(bytes).ok()
}

/// File extension for a downloaded media file.
pub fn extension(kind: MsgKind, mime: Option<&str>, file_name: Option<&str>) -> String {
    if let Some(ext) = file_name.and_then(|n| std::path::Path::new(n).extension()).and_then(|e| e.to_str()) {
        return ext.to_lowercase();
    }
    let mime = mime.unwrap_or("").split(';').next().unwrap_or("").trim();
    match mime {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "audio/ogg" => "ogg",
        "audio/mpeg" => "mp3",
        "audio/mp4" | "audio/aac" => "m4a",
        "application/pdf" => "pdf",
        _ => match kind {
            MsgKind::Image => "jpg",
            MsgKind::Video => "mp4",
            MsgKind::Audio | MsgKind::Voice => "ogg",
            MsgKind::Sticker => "webp",
            _ => "bin",
        },
    }
    .to_string()
}
