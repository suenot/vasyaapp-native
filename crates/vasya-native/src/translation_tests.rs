//! Translation actor regressions; requests are aborted before yielding and replies injected.
use super::*;

async fn controller() -> (tempfile::TempDir, Controller, DialogKey) {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Backend::new(dir.path().into()).await.unwrap());
    let (tx, _) = watch::channel(Arc::new(ViewModel::default()));
    let mut c = Controller::new(backend, tx, dir.path().into());
    let key = DialogKey {
        account: "account-a".into(),
        chat: 10,
        topic: Some(7),
    };
    c.vm.selected_account = Some(key.account.clone());
    c.vm.selected_chat = Some(key.chat);
    c.vm.selected_topic = key.topic;
    c.settings = json!({"notifications":false,"chat_translation":{}});
    let preference_key = c.translation_preference_key(&key);
    c.settings["chat_translation"][preference_key] = json!({
        "incoming_enabled":true,"outgoing_enabled":true,
        "incoming_target":"French","outgoing_target":"Chinese"
    });
    (dir, c, key)
}
fn draft(c: &mut Controller, text: &str) {
    c.vm.draft = text.into();
    c.translation.draft_revision += 1;
    c.save_translation_draft();
}
fn respond(c: &mut Controller, query: Query, result: Result<Value>) {
    c.reply(Reply {
        epoch: c.epoch,
        query,
        result,
        cached: false,
    });
    c.abort_requests();
}
fn incoming(id: i32, text: &str) -> MessageView {
    MessageView {
        id,
        text: text.into(),
        ..Default::default()
    }
}
async fn finish(c: &Controller) {
    c.abort_requests();
    c.backend.shutdown().await;
}

#[tokio::test]
async fn outgoing_translation_precedes_delivery_and_preserves_newer_draft() {
    let (_dir, mut c, key) = controller().await;
    draft(&mut c, "Original 42");
    c.begin_send(SendPayload::Text, None).unwrap();
    c.abort_requests();
    let intent = c.translation.sending[&key].clone();
    assert!(intent.translating);
    assert_eq!(intent.original, "Original 42");
    assert!(c.histories.get(&key).is_none_or(Vec::is_empty));
    assert_eq!(c.vm.draft, "Original 42");
    respond(
        &mut c,
        Query::TranslationOutgoing(Box::new(intent)),
        Ok(json!({"text":"译文 42"})),
    );
    let delivery = c.translation.sending[&key].clone();
    assert!(!delivery.translating);
    assert_eq!(c.histories[&key][0].text, "译文 42");
    assert!(c.histories[&key][0].pending);
    draft(&mut c, "New draft while delivery is pending");
    respond(
        &mut c,
        Query::Delivery(Box::new(delivery)),
        Ok(json!({"id":501,"text":"译文 42","is_outgoing":true})),
    );
    assert_eq!(c.vm.draft, "New draft while delivery is pending");
    assert_eq!(
        c.translation.drafts[&(c.translation.scope.clone(), key.clone())].text,
        c.vm.draft
    );
    assert_eq!(c.histories[&key].len(), 1);
    assert_eq!(c.histories[&key][0].id, 501);
    assert!(c.translation.sending.is_empty());
    finish(&c).await;
}

#[tokio::test]
async fn failure_empty_result_and_changed_provider_never_deliver_original() {
    let (_dir, mut c, key) = controller().await;
    for case in 0..3 {
        draft(&mut c, "Keep this original draft");
        c.begin_send(SendPayload::Text, None).unwrap();
        c.abort_requests();
        let intent = c.translation.sending[&key].clone();
        if case == 2 {
            c.translation.generation += 1;
        }
        let result = match case {
            0 => Err(anyhow!("Provider unavailable")),
            1 => Ok(json!({"text":"  "})),
            _ => Ok(json!({"text":"Old provider result"})),
        };
        respond(&mut c, Query::TranslationOutgoing(Box::new(intent)), result);
        assert!(c.translation.sending.is_empty());
        assert!(c.histories.get(&key).is_none_or(Vec::is_empty));
        assert_eq!(c.vm.draft, "Keep this original draft");
        assert!(c.vm.error.is_some());
    }
    finish(&c).await;
}

#[tokio::test]
async fn outgoing_ack_after_navigation_clears_only_original_scoped_draft() {
    let (_dir, mut c, key) = controller().await;
    draft(&mut c, "Send in first chat");
    c.begin_send(SendPayload::Text, None).unwrap();
    c.abort_requests();
    let intent = c.translation.sending[&key].clone();
    c.vm.selected_chat = Some(20);
    c.vm.selected_topic = None;
    c.restore_translation_draft();
    draft(&mut c, "Draft in second chat");
    respond(
        &mut c,
        Query::TranslationOutgoing(Box::new(intent)),
        Ok(json!({"text":"Translated first chat"})),
    );
    assert!(c.histories.get(&c.key().unwrap()).is_none_or(Vec::is_empty));
    let delivery = c.translation.sending[&key].clone();
    assert_eq!(delivery.key, key);
    respond(
        &mut c,
        Query::Delivery(Box::new(delivery)),
        Ok(json!({"id":502,"text":"Translated first chat"})),
    );
    assert_eq!(c.vm.draft, "Draft in second chat");
    assert!(
        c.translation.drafts[&(c.translation.scope.clone(), key.clone())]
            .text
            .is_empty()
    );
    assert_eq!(c.histories[&key][0].text, "Translated first chat");
    finish(&c).await;
}

#[tokio::test]
async fn viewport_work_is_bounded_and_cache_toggle_keeps_original_authoritative() {
    let (_dir, mut c, key) = controller().await;
    c.histories.insert(
        key.clone(),
        vec![
            incoming(1, "Original one"),
            incoming(2, "Original two"),
            incoming(3, "Original three"),
            incoming(4, "Hidden message"),
        ],
    );
    c.translation.visible_dialog = Some(key.clone());
    c.translation.visible = vec![1, 2, 3];
    c.pump_translations();
    c.abort_requests();
    assert_eq!(c.translation.active.len(), 2);
    assert!(c.translation.active.iter().all(|k| k.id <= 3));
    let first = c
        .translation
        .active
        .iter()
        .find(|k| k.id == 1)
        .unwrap()
        .clone();
    respond(
        &mut c,
        Query::TranslationIncoming(first.clone()),
        Ok(json!({"text":"Bonjour un"})),
    );
    c.pump_translations();
    c.abort_requests();
    assert_eq!(c.translation.active.len(), 2);
    assert!(c.translation.active.iter().any(|k| k.id == 3));
    c.show_history();
    c.publish();
    c.abort_requests();
    assert_eq!(c.vm.messages[0].text, "Original one");
    assert_eq!(c.vm.messages[0].translation.as_deref(), Some("Bonjour un"));
    c.translation_action(key.clone(), 1, false);
    c.decorate_translation();
    assert!(c.vm.messages[0].translation_show_original);
    assert_eq!(c.histories[&key][0].text, "Original one");
    c.translation_action(key.clone(), 1, false);
    c.decorate_translation();
    assert!(!c.vm.messages[0].translation_show_original);
    assert_eq!(c.translation.cache[&first].as_ref().unwrap(), "Bonjour un");
    c.pump_translations();
    c.abort_requests();
    assert!(!c.translation.active.contains(&first));
    finish(&c).await;
}

#[tokio::test]
async fn incoming_results_reject_edits_provider_changes_and_old_epochs() {
    let (_dir, mut c, key) = controller().await;
    c.histories
        .insert(key.clone(), vec![incoming(1, "Original")]);
    let old = c.incoming_key(&key, &c.histories[&key][0]).unwrap();
    c.histories.get_mut(&key).unwrap()[0].text = "Edited original".into();
    respond(
        &mut c,
        Query::TranslationIncoming(old.clone()),
        Ok(json!({"text":"Stale translation"})),
    );
    assert!(c.translation.cache.is_empty());
    let edited = c.incoming_key(&key, &c.histories[&key][0]).unwrap();
    c.translation.generation += 1;
    respond(
        &mut c,
        Query::TranslationIncoming(edited),
        Ok(json!({"text":"Old provider"})),
    );
    assert!(c.translation.cache.is_empty());
    let current = c.incoming_key(&key, &c.histories[&key][0]).unwrap();
    c.reply(Reply {
        epoch: c.epoch.wrapping_sub(1),
        query: Query::TranslationIncoming(current),
        result: Ok(json!({"text":"Old session"})),
        cached: false,
    });
    assert!(c.translation.cache.is_empty());
    assert_eq!(c.histories[&key][0].text, "Edited original");
    finish(&c).await;
}

#[tokio::test]
async fn translation_preferences_inherit_topics_but_isolate_accounts_chats_and_servers() {
    let (_dir, mut c, key) = controller().await;
    let mut other = key.clone();
    other.topic = Some(8);
    assert!(c.translation_preferences(&other).incoming_enabled);
    other.account = "account-b".into();
    assert!(!c.translation_preferences(&other).incoming_enabled);
    other = key.clone();
    other.chat = 11;
    assert!(!c.translation_preferences(&other).outgoing_enabled);
    c.translation.scope = "remote:https://other.example".into();
    assert!(!c.translation_preferences(&key).incoming_enabled);
    finish(&c).await;
}

#[tokio::test]
async fn navigating_away_and_back_clears_unchanged_acknowledged_draft() {
    let (_dir, mut c, key) = controller().await;
    draft(&mut c, "First chat text");
    c.begin_send(SendPayload::Text, None).unwrap();
    c.abort_requests();
    let intent = c.translation.sending[&key].clone();
    c.vm.selected_chat = Some(20);
    c.vm.selected_topic = None;
    c.restore_translation_draft();
    draft(&mut c, "Second chat text raises revision");
    c.vm.selected_chat = Some(key.chat);
    c.vm.selected_topic = key.topic;
    c.restore_translation_draft();
    assert_eq!(c.vm.draft, "First chat text");
    respond(
        &mut c,
        Query::TranslationOutgoing(Box::new(intent)),
        Ok(json!({"text":"Translated first chat"})),
    );
    let delivery = c.translation.sending[&key].clone();
    respond(
        &mut c,
        Query::Delivery(Box::new(delivery)),
        Ok(json!({"id":503,"text":"Translated first chat"})),
    );
    assert!(
        c.vm.draft.is_empty(),
        "Acknowledged unchanged draft must not remain ready to resend"
    );
    finish(&c).await;
}

#[tokio::test]
async fn edited_message_uses_matching_cached_translation_among_old_versions() {
    let (_dir, mut c, key) = controller().await;
    // Keep several legitimate historical text variants for one message ID.
    // Every currently selected version must retrieve its own translation.
    let versions: Vec<_> = (0..16)
        .map(|index| format!("Original version {index}"))
        .collect();
    for (index, text) in versions.iter().enumerate() {
        c.histories.insert(key.clone(), vec![incoming(1, text)]);
        let request = c.incoming_key(&key, &c.histories[&key][0]).unwrap();
        respond(
            &mut c,
            Query::TranslationIncoming(request),
            Ok(json!({"text":format!("Translated version {index}")})),
        );
    }
    assert_eq!(c.translation.cache.len(), versions.len());
    for (index, text) in versions.iter().enumerate() {
        c.histories.insert(key.clone(), vec![incoming(1, text)]);
        c.show_history();
        c.publish();
        c.abort_requests();
        assert_eq!(c.vm.messages[0].text, *text);
        assert_eq!(
            c.vm.messages[0].translation.as_deref(),
            Some(format!("Translated version {index}").as_str())
        );
    }
    finish(&c).await;
}
