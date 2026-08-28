use color_eyre::eyre::Result;
use tracing::error;

/// Installs the panic and error hooks. A panic restores the terminal before it prints,
/// so a crash never leaves the alternate screen behind.
pub fn init() -> Result<()> {
    let (panic_hook, eyre_hook) = color_eyre::config::HookBuilder::default()
        .panic_section(format!(
            "This is a bug. Consider reporting it at {}",
            env!("CARGO_PKG_REPOSITORY")
        ))
        .capture_span_trace_by_default(false)
        .display_location_section(false)
        .display_env_section(false)
        .into_hooks();
    eyre_hook.install()?;
    std::panic::set_hook(Box::new(move |panic_info| {
        if let Err(err) = crate::tui::restore() {
            error!("could not restore the terminal: {err}");
        }
        error!("panic: {panic_info}");
        eprintln!("{}", panic_hook.panic_report(panic_info));
        std::process::exit(1);
    }));
    Ok(())
}
