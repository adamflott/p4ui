//! Deterministic SVG screenshot from the real Ratatui renderer, using sample data.
use anyhow::Result;
use p4ui::{app::App, config::Config, model::*, ui};
use ratatui::{Terminal, backend::TestBackend, style::Color};
fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
fn color(c: Color) -> String {
    match c {
        Color::Black => "#171b22".into(),
        Color::Red | Color::LightRed => "#f7768e".into(),
        Color::Green | Color::LightGreen => "#9ece6a".into(),
        Color::Cyan | Color::LightCyan => "#7dcfff".into(),
        Color::Blue | Color::LightBlue => "#7aa2f7".into(),
        Color::Yellow | Color::LightYellow => "#e0af68".into(),
        Color::Magenta | Color::LightMagenta => "#bb9af7".into(),
        Color::Gray | Color::White => "#c0caf5".into(),
        Color::DarkGray => "#565f89".into(),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        _ => "#c0caf5".into(),
    }
}
fn main() -> Result<()> {
    let mut app = App::for_view(
        Config::default(),
        View {
            info: Info {
                user: "alice".into(),
                client: "alice-workspace".into(),
                root: "/workspace".into(),
                server: "perforce:1666".into(),
                stream: "//project/main".into(),
            },
            items: vec![
                Item::File(FileEntry {
                    depot: "//project/main/src/client.rs".into(),
                    local: "/workspace/src/client.rs".into(),
                    action: "edit".into(),
                    change: "1042".into(),
                    opened: true,
                    ..Default::default()
                }),
                Item::File(FileEntry {
                    depot: "//project/main/tests/client.rs".into(),
                    local: "/workspace/tests/client.rs".into(),
                    action: "add".into(),
                    change: "1042".into(),
                    opened: true,
                    ..Default::default()
                }),
                Item::File(FileEntry {
                    depot: "//project/main/README.md".into(),
                    local: "/workspace/README.md".into(),
                    action: "edit".into(),
                    ..Default::default()
                }),
                Item::File(FileEntry {
                    depot: "//project/main/docs/setup.md".into(),
                    local: "/workspace/docs/setup.md".into(),
                    action: "add".into(),
                    ..Default::default()
                }),
            ],
            ..Default::default()
        },
    );
    app.target = "1042".into();
    app.status = "4 items · scope: current directory and descendants".into();
    app.detail_title = "//project/main/src/client.rs".into();
    app.detail="--- //project/main/src/client.rs#8\n+++ /workspace/src/client.rs\n@@ -12,6 +12,9 @@\n impl Client {\n     pub fn connect(&self) -> Result<Session> {\n-        self.open()\n+        let session = self.open()?;\n+        session.verify_workspace()?;\n+        Ok(session)\n     }\n }".into();
    let (w, h) = (140u16, 32u16);
    let mut terminal = Terminal::new(TestBackend::new(w, h))?;
    terminal.draw(|f| ui::draw(f, &app))?;
    println!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\" role=\"img\" aria-label=\"p4ui sample status view\">",
        w * 8 + 24,
        h * 18 + 24,
        w * 8 + 24,
        h * 18 + 24
    );
    println!(
        "<rect width=\"100%\" height=\"100%\" rx=\"8\" fill=\"#171b22\"/><g font-family=\"Menlo,Consolas,monospace\" font-size=\"13\">"
    );
    let buffer = terminal.backend().buffer();
    for y in 0..h {
        for x in 0..w {
            if let Some(cell) = buffer.cell((x, y)) {
                let symbol = cell.symbol();
                let px = 12 + x * 8;
                let py = 12 + y * 18;
                if cell.bg != Color::Reset {
                    println!(
                        "<rect x=\"{px}\" y=\"{py}\" width=\"8\" height=\"18\" fill=\"{}\"/>",
                        color(cell.bg)
                    );
                }
                if !symbol.trim().is_empty() {
                    println!(
                        "<text x=\"{px}\" y=\"{}\" fill=\"{}\">{}</text>",
                        py + 14,
                        color(cell.fg),
                        xml(symbol)
                    );
                }
            }
        }
    }
    println!("</g></svg>");
    Ok(())
}
