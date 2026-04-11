use std::{collections::HashMap, hash::Hash, path::Path, sync::Arc};

use chrono::{Datelike, Local, NaiveTime, TimeDelta, Weekday};
use matrix_sdk::{
    Client, Error, LoopCtrl, Room, RoomState,
    config::SyncSettings,
    ruma::{
        OwnedEventId, OwnedRoomId, OwnedUserId,
        api::client::filter::FilterDefinition,
        events::{
            reaction::{OriginalSyncReactionEvent, ReactionEventContent},
            relation::Annotation,
            room::{
                message::{
                    MessageType, OriginalSyncRoomMessageEvent, ReplacementMetadata, RoomMessageEventContent,
                },
                redaction::OriginalSyncRoomRedactionEvent,
            },
        },
    },
};
use strum::{EnumIter, IntoEnumIterator};
use tokio::{
    sync::Mutex,
    task::JoinHandle,
    time::{Instant, MissedTickBehavior},
};
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use crate::persist_session::{login, persist_sync_token, restore_session};

mod crous;
mod emoji_verification;
mod persist_session;
mod poll;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    //https://www.tchap.gouv.fr/#/room/!jjgrGIYRRNERhDlWrU:agent.education.tchap.gouv.fr
    dotenvy::dotenv()?;

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    let data_dir = dirs::data_dir()
        .expect("no data_dir directory found")
        .join("matrix-crous-bot");
    let session_file = data_dir.join("session");

    let (client, sync_token) = if session_file.exists() {
        restore_session(&session_file).await?
    } else {
        (login(&data_dir, &session_file).await?, None)
    };
    info!("session started");

    sync(client, sync_token, &session_file).await
}

#[derive(Debug, Clone, Copy, EnumIter)]
pub enum PollSelection {
    LabriWithFood,
    LabriBuyFood,
    Crous,
    Cnrs,
    Other,
}

impl PollSelection {
    pub fn as_emoji(&self) -> String {
        match self {
            Self::LabriWithFood => "1️⃣",
            Self::LabriBuyFood => "2️⃣",
            Self::Crous => "3️⃣",
            Self::Cnrs => "4️⃣",
            Self::Other => "5️⃣",
        }
        .to_string()
    }
}

pub struct TryFromStringError;
impl TryFrom<String> for PollSelection {
    type Error = TryFromStringError;

    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        match &value[..] {
            "1️⃣" => Ok(Self::LabriWithFood),
            "2️⃣" => Ok(Self::LabriBuyFood),
            "3️⃣" => Ok(Self::Crous),
            "4️⃣" => Ok(Self::Cnrs),
            "5️⃣" => Ok(Self::Other),
            _ => Err(TryFromStringError),
        }
    }
}

#[derive(Debug, Default)]
pub struct BotData {
    pub polls: HashMap<OwnedRoomId, PollData>,
    pub auto_polls: HashMap<OwnedRoomId, JoinHandle<()>>,
}

#[derive(Debug, Clone)]
pub struct PollData {
    pub poll_event_id: OwnedEventId,
    // selections: HashMap<OwnedUserId, Vec<(OwnedEventId, String)>>
    pub labri_buy_food: HashMap<OwnedUserId, OwnedEventId>,
    pub labri_with_food: HashMap<OwnedUserId, OwnedEventId>,
    pub crous: HashMap<OwnedUserId, OwnedEventId>,
    pub cnrs: HashMap<OwnedUserId, OwnedEventId>,
    pub other: HashMap<OwnedUserId, OwnedEventId>,
}

impl PollData {
    fn new(poll_event_id: OwnedEventId) -> Self {
        Self {
            poll_event_id,
            labri_buy_food: HashMap::new(),
            labri_with_food: HashMap::new(),
            crous: HashMap::new(),
            cnrs: HashMap::new(),
            other: HashMap::new(),
        }
    }

    /// Remove the selection of a user, by its event id
    /// Returns true if the event was stored and the poll message should be updated
    fn remove_selection_by_id(&mut self, event_id: &OwnedEventId) -> Option<(OwnedUserId, OwnedEventId)> {
        self.labri_buy_food
            .remove_by_value(event_id)
            .or_else(|| self.labri_with_food.remove_by_value(event_id))
            .or_else(|| self.crous.remove_by_value(event_id))
            .or_else(|| self.cnrs.remove_by_value(event_id))
            .or_else(|| self.other.remove_by_value(event_id))
    }

    /// Remove the selection from the user.
    /// Returns the event id to redact.
    fn remove_selection_for_user(&mut self, user_id: &OwnedUserId) -> Option<OwnedEventId> {
        self.labri_buy_food
            .remove(user_id)
            .or_else(|| self.labri_with_food.remove(user_id))
            .or_else(|| self.crous.remove(user_id))
            .or_else(|| self.cnrs.remove(user_id))
            .or_else(|| self.other.remove(user_id))
    }

    /// User selected an option.
    /// Return the event associated to the previous selection, if any.
    fn user_select(
        &mut self,
        selection: PollSelection,
        user_id: OwnedUserId,
        event_id: OwnedEventId,
    ) -> Option<OwnedEventId> {
        let e = self.remove_selection_for_user(&user_id);
        match selection {
            PollSelection::LabriWithFood => self.labri_with_food.insert(user_id, event_id),
            PollSelection::LabriBuyFood => self.labri_buy_food.insert(user_id, event_id),
            PollSelection::Crous => self.crous.insert(user_id, event_id),
            PollSelection::Cnrs => self.cnrs.insert(user_id, event_id),
            PollSelection::Other => self.other.insert(user_id, event_id),
        };
        e
    }

    // /// User unselected on an option.
    // fn user_unselect(&mut self, user_id: &OwnedUserId) {
    //     self.remove_selection_for_user(user_id);
    // }
}

const BOT_PREFIX: &str = "[poll-bot]";

/// Setup the client to listen to new messages.
async fn sync(client: Client, initial_sync_token: Option<String>, session_file: &Path) -> anyhow::Result<()> {
    let user_id = client.user_id().unwrap().to_owned();
    info!("Launching a first sync to ignore past messages…");

    // Enable room members lazy-loading, it will speed up the initial sync a lot
    // with accounts in lots of rooms.
    // See <https://spec.matrix.org/v1.6/client-server-api/#lazy-loading-room-members>.
    let filter = FilterDefinition::with_lazy_loading();

    let mut sync_settings = SyncSettings::default().filter(filter.into());

    // We restore the sync where we left.
    // This is not necessary when not using `sync_once`. The other sync methods get
    // the sync token from the store.
    if let Some(sync_token) = initial_sync_token {
        sync_settings = sync_settings.token(sync_token);
    }

    // Let's ignore messages before the program was launched.
    // This is a loop in case the initial sync is longer than our timeout. The
    // server should cache the response and it will ultimately take less time to
    // receive.
    loop {
        match client.sync_once(sync_settings.clone()).await {
            Ok(response) => {
                // This is the last time we need to provide this token, the sync method after
                // will handle it on its own.
                sync_settings = sync_settings.token(response.next_batch.clone());
                persist_sync_token(session_file, response.next_batch).await?;
                break;
            }
            Err(error) => {
                error!("An error occurred during initial sync: {error}");
                error!("Trying again…");
            }
        }
    }

    emoji_verification::setup_device_verification(&client);

    let data = Arc::new(Mutex::new(BotData::default()));

    let data1 = data.clone();
    let user_id1 = user_id.clone();
    // let client1 = client.clone();
    client.add_event_handler(|ev: OriginalSyncRoomRedactionEvent, room: Room| async move {
        if ev.sender == user_id1 {
            // Ignore our redactions
            return;
        }
        if let Some(redacted_event) = ev.content.redacts {
            let mut data = data1.lock().await;
            if let Some(poll_data) = data.polls.get_mut(room.room_id())
                && poll_data.remove_selection_by_id(&redacted_event).is_some()
            {
                //update poll
                let poll_msg = poll::create_poll_message_with_data(poll_data.clone()).await;
                let replacement = poll_msg
                    .make_replacement(ReplacementMetadata::new(poll_data.poll_event_id.clone(), None));
                room.send(replacement).await.unwrap();
            }
        }
    });

    let data2 = data.clone();
    let user_id2 = user_id.clone();
    // let client2 = client.clone();
    client.add_event_handler(|ev: OriginalSyncReactionEvent, room: Room| async move {
        if ev.sender == user_id2 {
            // Ignore our reactions
            return;
        }
        let message_id = ev.content.relates_to.event_id;
        let mut data = data2.lock().await;

        if let Some(poll_data) = data.polls.get_mut(room.room_id())
            && poll_data.poll_event_id == message_id
        {
            let emoji = ev.content.relates_to.key;
            if let Ok(selection) = PollSelection::try_from(emoji) {
                let previous_selection = poll_data.user_select(selection, ev.sender, ev.event_id);
                if let Some(e) = previous_selection {
                    let _ = room
                        .redact(&e, Some(format!("{BOT_PREFIX} emoji unselection").as_str()), None)
                        .await;
                    // info!("sent emoji redaction");
                }
                let poll_msg = poll::create_poll_message_with_data(poll_data.clone()).await;
                let replacement = poll_msg.make_replacement(ReplacementMetadata::new(message_id, None));
                room.send(replacement).await.unwrap();
            }
        }
        // info!("user emoji select {:?}", data);
    });

    // let mut auto_poll_threads: Vec<_> = vec![];

    // Now that we've synced, let's attach a handler for incoming room messages.
    // let data3 = data.clone();
    // let client3 = client.clone();
    client.add_event_handler(|event: OriginalSyncRoomMessageEvent, room: Room| async move {
        // We only want to log text messages in joined rooms.
        if room.state() != RoomState::Joined {
            return;
        }
        if event.sender == user_id {
            // Ignore our reactions
            return;
        }
        let MessageType::Text(text_content) = &event.content.msgtype else {
            return;
        };

        if !text_content.body.starts_with("!") {
            return;
        }
        if let Some(command) = text_content.body.strip_prefix("!") {
            let command: Vec<&str> = command.split_whitespace().collect();
            match command[0] {
                "auto-poll" => {
                    // !auto-poll (true|false) <hour> +no_we
                    if command.len() < 2 {
                        let _ = room
                            .send(RoomMessageEventContent::text_plain("Missing start or stop"))
                            .await;
                        return;
                    }
                    match command[1] {
                        "start" => {
                            if command.len() < 3 {
                                let _ = room
                                    .send(RoomMessageEventContent::text_plain(
                                        "Missing the time to send the poll",
                                    ))
                                    .await;
                                return;
                            }
                            if data.lock().await.auto_polls.contains_key(room.room_id()) {
                                let _ = room
                                    .send(RoomMessageEventContent::text_plain(
                                        "An auto-poll is already scheduled for this room.",
                                    ))
                                    .await;
                                return;
                            }
                            match NaiveTime::parse_from_str(command[2], "%H:%M") {
                                Ok(wanted_time) => {
                                    // valid duration, setup the auto poll
                                    let now = Local::now();
                                    let mut start = now
                                        .date_naive()
                                        .and_time(wanted_time)
                                        .signed_duration_since(now.naive_local());
                                    // time is after today's poll, start with tomorrow's poll
                                    if start < TimeDelta::seconds(0) {
                                        start += TimeDelta::days(1);
                                    }
                                    let one_day = TimeDelta::days(1).to_std().unwrap();
                                    // let one_day = TimeDelta::days(1).to_std().unwrap();
                                    let mut interval = tokio::time::interval_at(
                                        Instant::now() + start.to_std().unwrap(),
                                        one_day,
                                    );
                                    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
                                    // let loop_data = data.clone();
                                    let skip_weekend = command.get(3).is_some_and(|&s| s == "+no_we");
                                    let cloned_room = room.clone();
                                    let handle = tokio::spawn(async move {
                                        let room = cloned_room;
                                        loop {
                                            interval.tick().await;
                                            if skip_weekend
                                                && matches!(
                                                    Local::now().weekday(),
                                                    Weekday::Sat | Weekday::Sun
                                                )
                                            {
                                                // it is the weekend and we skip it
                                                continue;
                                            }
                                            // it is the expected time of the day, send the poll
                                            // let bot_data = loop_data.lock().await;
                                            let _ = room
                                                .send(RoomMessageEventContent::text_plain("todays poll"))
                                                .await;
                                        }
                                    });
                                    data.lock()
                                        .await
                                        .auto_polls
                                        .insert(room.room_id().to_owned(), handle);
                                    let _ = room
                                        .send(RoomMessageEventContent::text_plain(format!(
                                            "Scheduled a poll every day at {}",
                                            command[2]
                                        )))
                                        .await;
                                }
                                Err(_) => {
                                    let _ = room
                                        .send(RoomMessageEventContent::text_plain(
                                            "Not a valid time. Please format is as %H:%M",
                                        ))
                                        .await;
                                }
                            }
                        }
                        "stop" => {
                            let mut bot_data = data.lock().await;
                            if let Some(handle) = bot_data.auto_polls.remove(room.room_id()) {
                                handle.abort();
                                let _ = handle.await;
                            }
                            let _ = room
                                .send(RoomMessageEventContent::text_plain("Auto poll stopped"))
                                .await;
                        }
                        _ => {
                            let _ = room
                                .send(RoomMessageEventContent::text_plain("Value is not start nor stop"))
                                .await;
                        }
                    }
                }
                "menu" => {
                    let content = poll::create_menu_message().await;
                    room.send(content).await.unwrap();
                }
                "poll" => {
                    let mut bot_data = data.lock().await;
                    if let Some(old_poll_data) = bot_data.polls.get(room.room_id()) {
                        // there was a poll already created
                        // wipe it and recreated it
                        // todo: future, if poll is of the current day, copy the old data or ignore the
                        // command
                        room.redact(
                            &old_poll_data.poll_event_id,
                            Some(&format!("{BOT_PREFIX} poll ended")),
                            None,
                        )
                        .await
                        .unwrap();
                    }
                    let content = poll::create_poll_message().await;
                    info!("sending poll");
                    let r = room.send(content).await.unwrap();
                    let poll_data = PollData::new(r.event_id.clone());
                    for poll_selection in PollSelection::iter() {
                        let reaction = ReactionEventContent::new(Annotation::new(
                            poll_data.poll_event_id.clone(),
                            poll_selection.as_emoji(),
                        ));
                        room.send(reaction).await.unwrap();
                    }
                    bot_data.polls.insert(room.room_id().to_owned(), poll_data);
                }
                "clearcache" => {
                    crous::clear_cache().await;
                }
                "html" => {
                    let content = RoomMessageEventContent::text_html(
                        "[bot] debug display of supported html tags",
                        HTML_DEBUG_MSG,
                    );
                    // RoomMessageEventContent::emote_plain(":pray:");
                    info!("sending message");
                    room.send(content).await.unwrap();
                    info!("message sent");
                }
                _ => {}
            }
        }
    });

    info!("The client is ready! Listening to new messages…");
    // This loops until we kill the program or an error happens.
    match client
        .sync_with_result_callback(sync_settings, |sync_result| async move {
            let response = match sync_result {
                Ok(it) => it,
                Err(err) => {
                    error!("error in the sync_result: {}", err);
                    return Err(err);
                }
            };

            // We persist the token each time to be able to restore our session
            match persist_sync_token(session_file, response.next_batch).await {
                Ok(_) => {}
                Err(err) => {
                    error!("error in the persist sync token: {}", err);
                    return Err(Error::UnknownError(err.into()));
                }
            }

            Ok(LoopCtrl::Continue)
        })
        .await
    {
        Ok(it) => it,
        Err(err) => {
            error!("error in the sync with result callback: {}", err);
            return Err(err.into());
        }
    };

    Ok(())
}

trait HashMapExt<K, V> {
    fn remove_by_value(&mut self, value: &V) -> Option<(K, V)>;
}

impl<K, V> HashMapExt<K, V> for HashMap<K, V>
where
    K: Eq + Hash + Clone,
    V: PartialEq,
{
    fn remove_by_value(&mut self, value: &V) -> Option<(K, V)> {
        let key = self.iter().find(|(_, v)| *v == value).map(|(k, _)| k.clone())?;

        self.remove_entry(&key)
    }
}

const HTML_DEBUG_MSG: &str = r#"
<h1>h1</h1>
<h2>h2</h2>
<h3>h3</h3>
<h4>h4</h4>
<h5>h5</h5>
<h6>h6</h6>
<blockquote>blockquote</blockquote>
<p>text in p</p><br>
<a href="https://google.com">a link</a>
<ul><li>item1</li><li>item2</li></ul>
<ol><li>item1</li><li>item2</li></ol>
<sup>sup</sup>
<sub>sub</sub>
<b>bold</b>
<i>italic</i>
<u>underline</u>
<strong>strong</strong>
<em>emphasis</em>
<s>strikethrough</s>
<code>code</code>
<hr>
<div>div</div>
<table>
  <caption>
    Formation développeur·euse front-end 2021
  </caption>
  <thead>
    <tr><th>Nom</th><th>Principal intérêt</th><th>Âge</th></tr>
  </thead>
  <tbody>
    <tr><th>Chris</th><td>Tables HTML</td><td>22</td></tr>
    <tr><th>Dennis</th><td>Accessibilité web</td><td>45</td></tr>
    <tr><th>Sarah</th><td>Frameworks JavaScript</td><td>29</td> </tr>
    <tr><th>Karen</th><td>Performance web</td><td>36</td></tr>
  </tbody>
</table>
<pre>preformatted block
with multiline
</pre>
<span>span</span><br>
<img src="https://raw.githubusercontent.com/tokio-rs/tracing/main/assets/logo-type.png">an image</img>
<details>
  <summary>Détails</summary>
  Quelque chose d'assez discret pour passer inaperçu.
</details></br>
<span data-mx-spoiler>the spoilered message</span></br>
<span data-mx-maths="\sin(x)=\frac{a}{b}">sin(x)</span>
"#;
