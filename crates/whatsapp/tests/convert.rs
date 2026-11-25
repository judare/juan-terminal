use whatsapp::MsgKind;
use whatsapp::convert::{Parsed, parse};
use whatsapp_rust::prelude::*;

#[test]
fn text_reply_reaction_revoke() {
    let m = wa::Message::text("hola");
    match parse(&m, true) {
        Parsed::Message(c) => {
            assert_eq!(c.kind, Some(MsgKind::Text));
            assert_eq!(c.text, "hola");
            assert!(c.proto.is_none(), "no proto kept for text");
        }
        _ => panic!(),
    }
    let reply = wa::Message {
        extended_text_message: MessageField::some(wa::message::ExtendedTextMessage {
            text: Some("sí".into()),
            context_info: MessageField::some(wa::ContextInfo { stanza_id: Some("Q1".into()), participant: Some("1@s.whatsapp.net".into()), quoted_message: MessageField::some(wa::Message::text("¿vienes?")), ..Default::default() }),
            ..Default::default()
        }),
        ..Default::default()
    };
    match parse(&reply, false) {
        Parsed::Message(c) => {
            assert_eq!(c.quoted_id.as_deref(), Some("Q1"));
            assert_eq!(c.quoted_text.as_deref(), Some("¿vienes?"));
        }
        _ => panic!(),
    }
    let react = wa::Message {
        reaction_message: MessageField::some(wa::message::ReactionMessage { key: MessageField::some(wa::MessageKey { id: Some("T".into()), ..Default::default() }), text: Some("❤️".into()), ..Default::default() }),
        ..Default::default()
    };
    assert!(matches!(parse(&react, false), Parsed::Reaction { ref target, ref emoji } if target == "T" && emoji == "❤️"));
    let revoke = wa::Message {
        protocol_message: MessageField::some(wa::message::ProtocolMessage { key: MessageField::some(wa::MessageKey { id: Some("X".into()), ..Default::default() }), r#type: Some(wa::message::protocol_message::Type::REVOKE), ..Default::default() }),
        ..Default::default()
    };
    assert!(matches!(parse(&revoke, false), Parsed::Revoke { ref target } if target == "X"));
}

#[test]
fn media_keeps_proto() {
    let m = wa::Message {
        audio_message: MessageField::some(wa::message::AudioMessage { ptt: Some(true), seconds: Some(7), mimetype: Some("audio/ogg; codecs=opus".into()), ..Default::default() }),
        ..Default::default()
    };
    match parse(&m, true) {
        Parsed::Message(c) => {
            assert_eq!(c.kind, Some(MsgKind::Voice));
            assert_eq!(c.duration, Some(7));
            let back = whatsapp::convert::decode(c.proto.as_ref().unwrap()).unwrap();
            assert_eq!(back.audio_message.as_option().unwrap().seconds, Some(7));
        }
        _ => panic!(),
    }
}
