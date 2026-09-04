/// Everything that changes application state arrives as a Message, whether it came from
/// a key, a timer, or a worker thread holding a clone of the sender.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Tick,
    Render,
    Resize(u16, u16),
    Quit,
    Error(String),
}
