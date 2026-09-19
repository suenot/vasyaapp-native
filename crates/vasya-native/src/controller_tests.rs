//! Actor-level regressions driven directly, without Telegram credentials/network.
use super::*;

async fn controller() -> (tempfile::TempDir, Controller) {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Backend::new(dir.path().into()).await.unwrap());
    let (tx, _) = watch::channel(Arc::new(ViewModel::default()));
    let mut controller = Controller::new(backend, tx, dir.path().into());
    controller.settings = json!({"notifications":false});
    (dir, controller)
}
fn key(account: &str, chat: i64, topic: Option<i32>) -> DialogKey {
    DialogKey {
        account: account.into(),
        chat,
        topic,
    }
}
fn select(controller: &mut Controller, key: &DialogKey) {
    controller.vm.selected_account = Some(key.account.clone());
    controller.vm.selected_chat = Some(key.chat);
    controller.vm.selected_topic = key.topic;
}
fn message(id: i32, text: &str) -> Value {
    json!({"id":id,"text":text,"sender_name":"Test","date":1789776000,"is_outgoing":false})
}
fn reply(controller: &mut Controller, query: Query, value: Value, cached: bool) {
    controller.reply(Reply {
        epoch: controller.epoch,
        query,
        result: Ok(value),
        cached,
    });
}
async fn finish(controller: &Controller) {
    controller.abort_requests();
    controller.backend.shutdown().await;
}

#[tokio::test]
async fn old_account_chat_and_topic_replies_do_not_replace_current_history() {
    let (_dir, mut c) = controller().await;
    let selected = key("account-b", 20, Some(9));
    select(&mut c, &selected);
    c.histories.insert(
        selected.clone(),
        vec![message_view(&message(42, "current dialog"))],
    );
    c.show_history();
    c.publish();
    for stale in [
        key("account-a", 20, Some(9)),
        key("account-b", 10, Some(9)),
        key("account-b", 20, Some(8)),
    ] {
        reply(
            &mut c,
            Query::Messages(stale.clone(), false, 0),
            json!([message(11, "old response")]),
            false,
        );
        reply(
            &mut c,
            Query::Topics(stale.clone()),
            json!([{"id":8,"title":"old topic"}]),
            false,
        );
        c.publish();
        assert_eq!(c.vm.messages.len(), 1);
        assert_eq!(c.vm.messages[0].text, "current dialog");
        assert!(c.vm.topics.is_empty());
        assert_eq!(c.histories[&stale][0].text, "old response");
    }
    finish(&c).await;
}

#[tokio::test]
async fn send_ack_deduplicates_echo_and_preserves_newly_typed_draft() {
    let (_dir, mut c) = controller().await;
    let selected = key("a1", 1, None);
    select(&mut c, &selected);
    c.vm.draft = "first message".into();
    c.send_message(None).unwrap();
    // Abort the embedded call before yielding; acknowledgments are supplied below.
    c.abort_requests();
    assert!(c.vm.draft.is_empty());
    let pending = c.histories[&selected][0].id;
    assert!(pending < 0);
    c.vm.draft = "second message still being composed".into();
    c.histories
        .get_mut(&selected)
        .unwrap()
        .push(message_view(&message(50, "first message")));
    reply(
        &mut c,
        Query::Sent(selected.clone(), pending),
        message(50, "first message"),
        false,
    );
    c.publish();
    assert_eq!(c.vm.draft, "second message still being composed");
    assert_eq!(c.vm.messages.iter().filter(|m| m.id == 50).count(), 1);
    assert!(!c.vm.messages.iter().any(|m| m.pending || m.id == pending));
    finish(&c).await;
}

#[tokio::test]
async fn live_edits_and_deletes_win_over_inflight_history_response() {
    let (_dir, mut c) = controller().await;
    let selected = key("a1", 1, None);
    select(&mut c, &selected);
    c.histories.insert(
        selected.clone(),
        vec![
            message_view(&message(10, "old edit")),
            message_view(&message(11, "delete me")),
        ],
    );
    let started = c.event_sequence;
    c.event(Event {
        name: "telegram:message-edited".into(),
        payload: json!({"accountId":"a1","chatId":1,"id":10,"newText":"live edit"}),
    });
    c.event(Event {
        name: "telegram:message-deleted".into(),
        payload: json!({"accountId":"a1","chatId":1,"messageIds":[11]}),
    });
    reply(
        &mut c,
        Query::Messages(selected.clone(), false, started),
        json!([message(10, "stale edit"), message(11, "stale deletion")]),
        false,
    );
    c.publish();
    assert_eq!(c.vm.messages.len(), 1);
    assert_eq!(c.vm.messages[0].text, "live edit");
    finish(&c).await;
}

#[tokio::test]
async fn cache_requested_after_delete_must_not_resurrect_deleted_message() {
    let (_dir, mut c) = controller().await;
    let selected = key("a1", 1, None);
    select(&mut c, &selected);
    c.histories.insert(
        selected.clone(),
        vec![message_view(&message(10, "deleted"))],
    );
    c.event(Event {
        name: "telegram:message-deleted".into(),
        payload: json!({"accountId":"a1","chatId":1,"messageIds":[10]}),
    });
    let started_after_delete = c.event_sequence;
    reply(
        &mut c,
        Query::Messages(selected.clone(), false, started_after_delete),
        json!([message(10, "old disk cache")]),
        true,
    );
    c.publish();
    assert!(
        c.vm.messages.is_empty(),
        "cached history resurrected a known deleted message"
    );
    finish(&c).await;
}

#[tokio::test]
async fn account_wide_deletion_does_not_touch_other_accounts() {
    let (_dir, mut c) = controller().await;
    let first = key("a1", 1, None);
    let second = key("a2", 1, None);
    select(&mut c, &second);
    for k in [&first, &second] {
        c.histories.insert(
            k.clone(),
            vec![message_view(&message(12, "same numeric message ID"))],
        );
    }
    c.event(Event {
        name: "telegram:message-deleted".into(),
        payload: json!({"accountId":"a1","chatId":0,"messageIds":[12]}),
    });
    c.publish();
    assert!(c.histories[&first].is_empty());
    assert_eq!(c.vm.messages.len(), 1);
    assert_eq!(c.vm.messages[0].id, 12);
    finish(&c).await;
}

#[tokio::test]
async fn authoritative_chat_refresh_removes_absent_chats_but_keeps_newer_event() {
    let (_dir, mut c) = controller().await;
    c.vm.selected_account = Some("a1".into());
    c.chats.insert(
        "a1".into(),
        vec![
            chat_view(&json!({"id":1,"title":"kept"})),
            chat_view(&json!({"id":2,"title":"removed"})),
            chat_view(&json!({"id":3,"title":"event wins"})),
        ],
    );
    c.chat_events.insert(("a1".into(), 3), 7);
    reply(
        &mut c,
        Query::Chats("a1".into(), 6),
        json!([{"id":1,"title":"fresh"}]),
        false,
    );
    c.publish();
    assert_eq!(c.vm.chats.len(), 2);
    assert!(!c.vm.chats.iter().any(|chat| chat.id == 2));
    assert!(c.vm.chats.iter().any(|chat| chat.id == 3));
    finish(&c).await;
}

#[tokio::test]
async fn chat_request_burst_coalesces_per_account_and_recovers_after_failure() {
    let (_dir, mut c) = controller().await;
    for _ in 0..100 {
        c.load_chats("a1".into());
    }
    assert_eq!(c.loading_chats.len(), 1);
    assert_eq!(
        c.tasks.lock().unwrap().len(),
        2,
        "one chat request plus one folder request expected"
    );
    c.abort_requests();
    c.reply(Reply {
        epoch: c.epoch,
        query: Query::Chats("a1".into(), 0),
        result: Err(anyhow!("offline")),
        cached: false,
    });
    assert!(!c.loading_chats.contains("a1"));
    c.load_chats("a1".into());
    assert_eq!(c.tasks.lock().unwrap().len(), 2);
    finish(&c).await;
}

#[tokio::test]
async fn older_pagination_advances_at_history_limit_and_preserves_pending_send() {
    let (_dir, mut c) = controller().await;
    let selected = key("a1", 1, None);
    select(&mut c, &selected);
    let mut history: Vec<_> = (1001..=6000)
        .map(|id| message_view(&message(id, "existing")))
        .collect();
    history.push(MessageView {
        id: -1,
        text: "pending".into(),
        pending: true,
        ..Default::default()
    });
    c.histories.insert(selected.clone(), history);
    reply(
        &mut c,
        Query::Messages(selected.clone(), true, 0),
        json!((951..=1000)
            .map(|id| message(id, "older page"))
            .collect::<Vec<_>>()),
        false,
    );
    let history = &c.histories[&selected];
    assert_eq!(history.iter().filter(|m| m.id > 0).count(), 5000);
    assert_eq!(
        history.iter().filter(|m| m.id > 0).map(|m| m.id).min(),
        Some(951)
    );
    assert_eq!(
        history.iter().filter(|m| m.id > 0).map(|m| m.id).max(),
        Some(5950)
    );
    assert!(history.iter().any(|m| m.id == -1 && m.pending));
    reply(
        &mut c,
        Query::Messages(selected.clone(), true, 0),
        json!((901..=950)
            .map(|id| message(id, "next older page"))
            .collect::<Vec<_>>()),
        false,
    );
    assert_eq!(
        c.histories[&selected]
            .iter()
            .filter(|m| m.id > 0)
            .map(|m| m.id)
            .min(),
        Some(901)
    );
    // A latest refresh intentionally moves the bounded window back toward now.
    reply(
        &mut c,
        Query::Messages(selected.clone(), false, 0),
        json!((5951..=6000)
            .map(|id| message(id, "latest page"))
            .collect::<Vec<_>>()),
        false,
    );
    assert_eq!(
        c.histories[&selected]
            .iter()
            .filter(|m| m.id > 0)
            .map(|m| m.id)
            .max(),
        Some(6000)
    );
    assert!(c.histories[&selected].iter().any(|m| m.id == -1));
    finish(&c).await;
}

#[tokio::test]
async fn authoritative_page_removes_cached_only_ids_but_preserves_newer_events() {
    let (_dir, mut c) = controller().await;
    let selected = key("a1", 1, None);
    select(&mut c, &selected);
    reply(
        &mut c,
        Query::Messages(selected.clone(), true, 0),
        json!([
            message(10, "stale deleted row"),
            message(11, "kept"),
            message(12, "old cached text")
        ]),
        true,
    );
    assert_eq!(c.cached_pages.len(), 1);
    c.vm.accounts.push(AccountView {
        id: "a1".into(),
        title: "Test".into(),
    });
    c.event(Event {
        name: "telegram:message-edited".into(),
        payload: json!({"accountId":"a1","chatId":1,"id":12,"newText":"newer live edit"}),
    });
    reply(
        &mut c,
        Query::Messages(selected.clone(), true, 0),
        json!([message(11, "authoritative")]),
        false,
    );
    assert!(!c.histories[&selected].iter().any(|m| m.id == 10));
    assert_eq!(
        c.histories[&selected]
            .iter()
            .find(|m| m.id == 12)
            .unwrap()
            .text,
        "newer live edit"
    );
    assert!(c.cached_pages.is_empty());
    // A failed live refresh consumes bookkeeping without discarding cached data.
    reply(
        &mut c,
        Query::Messages(selected.clone(), true, 1),
        json!([message(9, "offline page")]),
        true,
    );
    c.reply(Reply {
        epoch: c.epoch,
        query: Query::Messages(selected.clone(), true, 1),
        result: Err(anyhow!("offline")),
        cached: false,
    });
    assert!(c.cached_pages.is_empty());
    assert!(c.histories[&selected].iter().any(|m| m.id == 9));
    finish(&c).await;
}

#[tokio::test]
async fn queued_viewport_from_other_account_chat_or_topic_has_no_media_side_effects() {
    let (_dir, mut c) = controller().await;
    let selected = key("current", 20, Some(9));
    select(&mut c, &selected);
    c.settings["auto_transcribe"] = json!(true);
    c.vm.preferences.auto_photos = true;
    c.vm.messages = Arc::new(vec![
        MessageView {
            id: 7,
            media_kind: Some("photo".into()),
            ..Default::default()
        },
        MessageView {
            id: 8,
            media_kind: Some("voice".into()),
            ..Default::default()
        },
    ]);
    for stale in [
        key("previous", 20, Some(9)),
        key("current", 19, Some(9)),
        key("current", 20, Some(8)),
    ] {
        c.command(UiCommand::VisibleMessages {
            account: stale.account,
            chat: stale.chat,
            topic: stale.topic,
            ids: vec![7, 8],
        })
        .unwrap();
    }
    assert!(c.downloading.is_empty());
    assert!(c.transcribing.is_empty());
    assert!(c.tasks.lock().unwrap().is_empty());
    c.command(UiCommand::VisibleMessages {
        account: selected.account.clone(),
        chat: selected.chat,
        topic: selected.topic,
        ids: vec![7, 8],
    })
    .unwrap();
    assert!(c.downloading.contains(&(selected.clone(), 7)));
    assert!(c.transcribing.contains(&(selected, 8)));
    assert_eq!(c.tasks.lock().unwrap().len(), 2);
    finish(&c).await;
}
