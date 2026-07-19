/// Configure zmux-owned storage and launch the graphical application.
///
/// Windows packages expose separate console and GUI entry points. Keeping the
/// shared startup here ensures both entry points resolve settings and database
/// state identically.
pub fn run_gui() -> anyhow::Result<()> {
    crate::bootstrap::configure_zmux_paths()?;
    crate::run()
}
