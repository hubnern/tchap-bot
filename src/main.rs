use std::{collections::HashMap, hash::Hash, path::Path, sync::Arc};

use matrix_sdk::{
    Client, Error, LoopCtrl, Room, RoomState,
    config::SyncSettings,
    ruma::{
        OwnedEventId, OwnedUserId,
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
use tokio::sync::Mutex;
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
    // dotenvy::dotenv()?;

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

#[derive(Debug, Default, Clone)]
pub struct BotData {
    pub poll_event_id: Option<OwnedEventId>,
    // selections: HashMap<OwnedUserId, Vec<(OwnedEventId, String)>>
    pub labri_buy_food: HashMap<OwnedUserId, OwnedEventId>,
    pub labri_with_food: HashMap<OwnedUserId, OwnedEventId>,
    pub crous: HashMap<OwnedUserId, OwnedEventId>,
    pub cnrs: HashMap<OwnedUserId, OwnedEventId>,
    pub other: HashMap<OwnedUserId, OwnedEventId>,
}

impl BotData {
    fn reset(&mut self, poll_event_id: &OwnedEventId) -> Option<OwnedEventId> {
        let old_poll_event = self.poll_event_id.clone();
        self.poll_event_id = Some(poll_event_id.clone());
        self.labri_buy_food = HashMap::new();
        self.labri_with_food = HashMap::new();
        self.crous = HashMap::new();
        self.cnrs = HashMap::new();
        self.other = HashMap::new();
        old_poll_event
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
            if data.remove_selection_by_id(&redacted_event).is_some() {
                //update poll
                let poll_msg = poll::create_poll_message(data.clone()).await;
                let replacement = poll_msg.make_replacement(ReplacementMetadata::new(
                    data.poll_event_id.clone().unwrap(),
                    None,
                ));
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
        if let Some(msg_id) = &data.poll_event_id
            && msg_id == &message_id
        {
            let emoji = ev.content.relates_to.key;
            if let Ok(selection) = PollSelection::try_from(emoji) {
                let previous_selection = data.user_select(selection, ev.sender, ev.event_id);
                if let Some(e) = previous_selection {
                    let _ = room
                        .redact(&e, Some(format!("{BOT_PREFIX} emoji unselection").as_str()), None)
                        .await;
                    // info!("sent emoji redaction");
                }
                let poll_msg = poll::create_poll_message(data.clone()).await;
                let replacement = poll_msg.make_replacement(ReplacementMetadata::new(message_id, None));
                room.send(replacement).await.unwrap();
            }
        }
        // info!("user emoji select {:?}", data);
    });

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

        match text_content.body.as_str() {
            "!auto-poll" => {
                // ]]auto-poll (true|false) <hour>
                todo!("send the poll every day at x hour");
            }
            "!menu" => {
                let content = poll::create_menu_message().await;
                room.send(content).await.unwrap();
            }
            "!poll" => {
                let content = poll::create_poll_message(data.lock().await.clone()).await;
                info!("sending poll");
                let r = room.send(content).await.unwrap();
                let poll_event_id = r.event_id;
                if let Some(old_poll_event) = data.lock().await.reset(&poll_event_id) {
                    room.redact(&old_poll_event, Some(&format!("{BOT_PREFIX} poll ended")), None)
                        .await
                        .unwrap();
                }
                for poll_selection in PollSelection::iter() {
                    let reaction = ReactionEventContent::new(Annotation::new(
                        poll_event_id.clone(),
                        poll_selection.as_emoji(),
                    ));
                    room.send(reaction).await.unwrap();
                }
            }
            "!html" => {
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
