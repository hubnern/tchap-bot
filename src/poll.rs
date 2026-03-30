use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;

pub fn create_poll() -> RoomMessageEventContent {
    RoomMessageEventContent::text_html("the simple body", "The html formatted body")
}
