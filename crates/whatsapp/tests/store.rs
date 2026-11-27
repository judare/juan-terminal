use whatsapp::store::{ChatMeta, NewMessage, Store};
use whatsapp::{MsgKind, MsgStatus};

fn tmp() -> std::path::PathBuf {
    // Tests run in parallel and the clock can repeat: a counter keeps each DB its own.
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("wa-test-{}-{n}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir_all(&d).unwrap();
    d.join("cache.db")
}

fn msg(chat: &str, id: &str, ts: i64, text: &str, from_me: bool, unseen: bool) -> NewMessage {
    NewMessage { chat: chat.into(), id: id.into(), sender: if from_me { "me".into() } else { chat.into() }, from_me, ts, kind: Some(MsgKind::Text), text: text.into(), unseen, ..Default::default() }
}

#[test]
fn chats_unread_and_order() {
    let st = Store::open(&tmp()).unwrap();
    st.upsert_chat("111@s.whatsapp.net", &ChatMeta { name: Some("Ana".into()), ..Default::default() });
    assert!(st.insert_message(&msg("111@s.whatsapp.net", "a1", 100, "hola", false, true)));
    assert!(!st.insert_message(&msg("111@s.whatsapp.net", "a1", 100, "hola", false, true)), "duplicate ignored");
    st.insert_message(&msg("222@s.whatsapp.net", "b1", 200, "qué tal", false, true));
    st.insert_message(&msg("222@s.whatsapp.net", "b2", 210, "ok", true, false));
    let chats = st.chats();
    assert_eq!(chats.len(), 2);
    assert_eq!(chats[0].jid, "222@s.whatsapp.net", "newest first");
    assert_eq!(chats[0].last_text, "ok");
    assert!(chats[0].last_from_me);
    assert_eq!(chats[1].name, "Ana");
    assert_eq!(chats[1].unread, 1);
    assert_eq!(chats[0].name, "+222", "phone label when nameless");
    let pending = st.mark_read("111@s.whatsapp.net");
    assert_eq!(pending, vec![("a1".to_string(), "111@s.whatsapp.net".to_string())]);
    assert_eq!(st.chat("111@s.whatsapp.net").unwrap().unread, 0);
    assert!(st.mark_read("111@s.whatsapp.net").is_empty());
}

#[test]
fn archived_and_hidden_are_excluded() {
    let st = Store::open(&tmp()).unwrap();
    st.insert_message(&msg("333@s.whatsapp.net", "c1", 10, "x", false, false));
    st.insert_message(&msg("status@broadcast", "s1", 20, "story", false, false));
    st.insert_message(&msg("123@newsletter", "n1", 30, "canal", false, false));
    st.upsert_chat("333@s.whatsapp.net", &ChatMeta { archived: Some(true), ..Default::default() });
    assert!(st.chats().is_empty());
    assert!(st.search("x", None, 10).is_empty(), "archived chats not searched");
}

#[test]
fn lid_alias_merges_chat() {
    let st = Store::open(&tmp()).unwrap();
    st.insert_message(&msg("999@lid", "l1", 50, "desde lid", false, true));
    st.insert_message(&msg("57300@s.whatsapp.net", "p1", 40, "desde pn", false, true));
    st.set_alias("999@lid", "57300@s.whatsapp.net");
    let chats = st.chats();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].jid, "57300@s.whatsapp.net");
    assert_eq!(chats[0].unread, 2);
    assert_eq!(chats[0].last_text, "desde lid");
    assert_eq!(st.canonical("999@lid"), "57300@s.whatsapp.net");
    assert_eq!(st.messages("57300@s.whatsapp.net", None, 10).len(), 2);
}

#[test]
fn search_status_reactions_resolve() {
    let st = Store::open(&tmp()).unwrap();
    st.upsert_chat("444@s.whatsapp.net", &ChatMeta { name: Some("Pedro Pérez".into()), ..Default::default() });
    st.insert_message(&msg("444@s.whatsapp.net", "m1", 1, "Reunión mañana 100% confirmada", true, false));
    st.insert_message(&msg("444@s.whatsapp.net", "m2", 2, "perfecto", false, false));
    assert_eq!(st.search("100%", None, 10).len(), 1, "% is literal");
    assert_eq!(st.search("REUNIÓN", Some("444@s.whatsapp.net"), 10).len(), 0, "LIKE is ASCII case-insensitive only");
    assert_eq!(st.search("reunión", Some("444@s.whatsapp.net"), 10).len(), 1);
    st.set_status("444@s.whatsapp.net", &["m1".into()], MsgStatus::Read);
    st.set_status("444@s.whatsapp.net", &["m1".into()], MsgStatus::Delivered);
    assert_eq!(st.message("444@s.whatsapp.net", "m1").unwrap().status, MsgStatus::Read, "status never goes back");
    st.set_reaction("444@s.whatsapp.net", "m1", "444@s.whatsapp.net", "👍", false);
    assert_eq!(st.message("444@s.whatsapp.net", "m1").unwrap().reactions.len(), 1);
    st.set_reaction("444@s.whatsapp.net", "m1", "444@s.whatsapp.net", "", false);
    assert!(st.message("444@s.whatsapp.net", "m1").unwrap().reactions.is_empty());
    assert_eq!(st.resolve_chat("pedro").as_deref(), Some("444@s.whatsapp.net"));
    assert_eq!(st.resolve_chat("+57 300 123 4567").as_deref(), Some("573001234567@s.whatsapp.net"));
    st.edit_text("444@s.whatsapp.net", "m2", "perfecto!!");
    let m = st.message("444@s.whatsapp.net", "m2").unwrap();
    assert!(m.edited && m.text == "perfecto!!");
    st.revoke("444@s.whatsapp.net", "m2");
    assert_eq!(st.message("444@s.whatsapp.net", "m2").unwrap().kind, MsgKind::Revoked);
    // Paging.
    let older = st.messages("444@s.whatsapp.net", Some(2), 10);
    assert_eq!(older.len(), 1);
    assert_eq!(older[0].id, "m1");
}

#[test]
fn read_only_handle_reads() {
    let p = tmp();
    let st = Store::open(&p).unwrap();
    st.insert_message(&msg("555@s.whatsapp.net", "r1", 5, "hola", false, false));
    let ro = Store::open_read_only(&p).unwrap();
    assert_eq!(ro.chats().len(), 1);
}

#[test]
fn labels_set_filter_and_follow_lid_merge() {
    let st = Store::open(&tmp()).unwrap();
    st.insert_message(&msg("111@s.whatsapp.net", "a1", 100, "hola", false, false));
    st.insert_message(&msg("9@lid", "l1", 150, "yo", false, false));
    let work = st.create_label("Trabajo").unwrap();
    assert_eq!(st.create_label(" trabajo ").unwrap(), work, "same name reuses the label");
    let fam = st.create_label("Familia").unwrap();
    assert!(st.create_label("  ").is_none());
    assert_eq!(st.labels().len(), 2);
    assert_ne!(st.labels()[0].color, st.labels()[1].color);
    st.set_chat_label("111@s.whatsapp.net", work, true);
    st.set_chat_label("111@s.whatsapp.net", fam, true);
    st.set_chat_label("111@s.whatsapp.net", work, true);
    st.set_chat_label("9@lid", work, true);
    assert_eq!(st.chat("111@s.whatsapp.net").unwrap().labels, vec![work, fam]);
    st.set_chat_label("111@s.whatsapp.net", fam, false);
    assert_eq!(st.chat("111@s.whatsapp.net").unwrap().labels, vec![work]);
    st.set_alias("9@lid", "999@s.whatsapp.net");
    assert_eq!(st.chat("999@s.whatsapp.net").unwrap().labels, vec![work], "label moves with the LID merge");
    st.delete_label(work);
    assert!(st.chat("111@s.whatsapp.net").unwrap().labels.is_empty());
    assert_eq!(st.labels().len(), 1);
}

#[test]
fn own_reply_clears_unread() {
    let st = Store::open(&tmp()).unwrap();
    let c = "444@s.whatsapp.net";
    st.insert_message(&msg(c, "i1", 100, "hola", false, true));
    st.insert_message(&msg(c, "i2", 110, "¿cómo vas?", false, true));
    // An old own message (history) doesn't clear newer incoming ones.
    st.insert_message(&msg(c, "o0", 90, "antes", true, false));
    assert_eq!(st.chat(c).unwrap().unread, 2);
    // Replying from the phone does.
    st.insert_message(&msg(c, "o1", 120, "bien", true, false));
    assert_eq!(st.chat(c).unwrap().unread, 0);
    assert!(st.mark_read(c).is_empty(), "incoming marked seen too");
}
