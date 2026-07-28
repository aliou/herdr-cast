mod api;
mod notify;
mod palette;

fn main() {
    let mut arguments = std::env::args().skip(1);
    let result = match arguments.next().as_deref() {
        Some("notify") if arguments.next().is_none() => notify::run(),
        Some("palette") if arguments.next().is_none() => palette::run(),
        Some("focus") => {
            let socket_path = arguments.next();
            let pane_id = arguments.next();
            match (socket_path, pane_id, arguments.next()) {
                (Some(socket_path), Some(pane_id), None) => notify::focus(&socket_path, &pane_id),
                _ => Err("usage: herdr-cast focus <socket-path> <pane-id>".to_string()),
            }
        }
        _ => Err("usage: herdr-cast <notify|palette|focus>".to_string()),
    };

    if let Err(error) = result {
        eprintln!("[cast] {error}");
        std::process::exit(1);
    }
}
