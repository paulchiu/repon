/// Everything that changes application state arrives as an Action, whether it came from
/// a key, a timer, or a worker thread holding a clone of the sender.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Tick,
    Render,
    Resize(u16, u16),
    Suspend,
    Resume,
    Quit,
    ClearScreen,
    Error(String),
}
