mod api;
mod notify;
mod palette;
mod picker;
mod workspace;
mod zoxide;

fn main() {
    let mut arguments = std::env::args().skip(1);
    let result = match arguments.next().as_deref() {
        Some("notify") if arguments.next().is_none() => notify::run(),
        Some("palette") if arguments.next().is_none() => palette::run(),
        Some("directory-workspace") if arguments.next().is_none() => {
            workspace::create_from_directory()
        }
        Some("workspace-picker") if arguments.next().is_none() => workspace::focus_existing(),
        Some("focus") => {
            let socket_path = arguments.next();
            let pane_id = arguments.next();
            match (socket_path, pane_id, arguments.next()) {
                (Some(socket_path), Some(pane_id), None) => notify::focus(&socket_path, &pane_id),
                _ => Err("usage: herdr-cast focus <socket-path> <pane-id>".to_string()),
            }
        }
        _ => Err(
            "usage: herdr-cast <notify|palette|directory-workspace|workspace-picker|focus>"
                .to_string(),
        ),
    };

    if let Err(error) = result {
        eprintln!("[cast] {error}");
        std::process::exit(1);
    }
}
