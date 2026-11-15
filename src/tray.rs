use crate::client::translate;
#[cfg(windows)]
use crate::ipc::Data;
#[cfg(windows)]
use hbb_common::tokio;
use hbb_common::{allow_err, config::PeerConfig, log};
use base::config::keys;
use std::sync::{Arc, Mutex};
#[cfg(windows)]
use std::time::Duration;

/// Maximum number of recent sessions listed in the tray menu.
const RECENT_LIMIT: usize = 5;
/// How often the recent session list is refreshed.
const RECENT_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
/// Menu id prefix of a recent session entry, the peer id follows it.
const RECENT_ITEM_ID_PREFIX: &str = "tray-recent:";
/// Menu id of the disabled entry shown when there is no recent session.
const RECENT_EMPTY_ITEM_ID: &str = "tray-recent-empty";
/// Maximum length of a recent session label.
const RECENT_LABEL_MAX_LEN: usize = 40;

/// A peer of a recent session, shown in the tray menu.
#[derive(Clone, PartialEq)]
struct RecentPeer {
    id: String,
    label: String,
}

/// The peers of the most recent outgoing sessions, newest first.
///
/// The source is the same as the "Recent sessions" tab of the main window,
/// the peer config files sorted by their modification time.
fn recent_peers() -> Vec<RecentPeer> {
    let list = PeerConfig::get_vec_id_modified_time_path(&None);
    let (peers, _) = PeerConfig::batch_peers(&list, 0, Some(RECENT_LIMIT));
    let mut peers: Vec<RecentPeer> = peers
        .into_iter()
        .filter(|(id, _, _)| is_url_safe_id(id))
        .map(|(id, _, config)| {
            let label = peer_label(&id, &config);
            RecentPeer { id, label }
        })
        .collect();
    disambiguate_labels(&mut peers);
    peers
}

/// The name of a peer, the same precedence as the peer cards of the main window.
fn peer_label(id: &str, config: &PeerConfig) -> String {
    let alias = config.options.get("alias").map(|s| s.trim()).unwrap_or("");
    if !alias.is_empty() {
        return alias.to_owned();
    }
    let hostname = config.info.hostname.trim();
    let username = config.info.username.trim();
    if !hostname.is_empty() {
        if !username.is_empty() {
            return format!("{username}@{hostname}");
        }
        return hostname.to_owned();
    }
    format_id(id)
}

/// Append the peer id to the entries that can not be told apart by their label.
fn disambiguate_labels(peers: &mut [RecentPeer]) {
    let labels: Vec<String> = peers.iter().map(|p| p.label.clone()).collect();
    for (i, peer) in peers.iter_mut().enumerate() {
        let key = normalize_label(&labels[i]);
        let duplicated = labels
            .iter()
            .filter(|l| normalize_label(l.as_str()) == key)
            .count()
            > 1;
        if duplicated && peer.label != format_id(&peer.id) {
            peer.label = format!("{} ({})", peer.label, format_id(&peer.id));
        }
    }
}

fn normalize_label(label: &str) -> String {
    label.trim().to_lowercase()
}

/// Group a numeric id in blocks of three, the way the main window shows it.
fn format_id(id: &str) -> String {
    let trimmed = id.trim();
    if trimmed.is_empty() || !trimmed.chars().all(|c| c.is_ascii_digit()) {
        return id.to_owned();
    }
    let chars: Vec<char> = trimmed.chars().collect();
    let len = chars.len();
    let first = if len % 3 == 0 { 3 } else { len % 3 };
    let mut formatted = String::with_capacity(len + len / 3);
    formatted.extend(chars[..first].iter());
    for chunk in chars[first..].chunks(3) {
        formatted.push(' ');
        formatted.extend(chunk.iter());
    }
    formatted
}

/// The peer id is sent to the main window in a url, an id that needs percent
/// encoding can not be decoded back by the url handler.
fn is_url_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Make a peer name fit a menu entry.
fn sanitize_menu_text(text: &str) -> String {
    let mut sanitized = text
        .replace('&', "&&")
        .replace('\n', " ")
        .replace('\r', " ")
        .replace('\t', " ");
    if sanitized.chars().count() > RECENT_LABEL_MAX_LEN {
        sanitized = sanitized.chars().take(RECENT_LABEL_MAX_LEN).collect();
        sanitized.push('…');
    }
    sanitized
}

/// Replace the entries of the recent session submenu.
fn update_recent_menu(submenu: &tray_icon::menu::Submenu, peers: &[RecentPeer]) {
    use tray_icon::menu::MenuItem;
    while submenu.remove_at(0).is_some() {}
    if peers.is_empty() {
        let item = MenuItem::with_id(
            RECENT_EMPTY_ITEM_ID,
            translate("No recent sessions".to_owned()),
            false,
            None,
        );
        submenu.append(&item).ok();
        return;
    }
    for peer in peers {
        let item = MenuItem::with_id(
            format!("{RECENT_ITEM_ID_PREFIX}{}", peer.id),
            sanitize_menu_text(&peer.label),
            true,
            None,
        );
        submenu.append(&item).ok();
    }
}

/// Hand a url over to the main window, it opens the connection it points to.
///
/// Returns false when there is no main window to receive it.
#[cfg(target_os = "linux")]
fn try_deliver_url(url: &str) -> bool {
    crate::dbus::invoke_new_connection(url.to_owned()).is_ok()
}

#[cfg(windows)]
fn try_deliver_url(url: &str) -> bool {
    use winapi::um::winuser::WM_USER;
    crate::platform::send_message_to_hnwd(
        &crate::platform::FLUTTER_RUNNER_WIN32_WINDOW_CLASS,
        &crate::get_app_name(),
        (WM_USER + 2) as _, // referred from unilinks desktop pub
        url,
        false,
    )
}

#[cfg(target_os = "macos")]
fn try_deliver_url(url: &str) -> bool {
    crate::ipc::send_url_scheme(url.to_owned()).is_ok()
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
fn try_deliver_url(_url: &str) -> bool {
    false
}

pub fn start_tray() {
    if crate::ui_interface::get_builtin_option(keys::OPTION_HIDE_TRAY) == "Y" {
        #[cfg(not(target_os = "macos"))]
        {
            return;
        }
    }

    #[cfg(target_os = "linux")]
    crate::server::check_zombie();

    allow_err!(make_tray());
}

fn make_tray() -> hbb_common::ResultType<()> {
    // https://github.com/tauri-apps/tray-icon/blob/dev/examples/tao.rs
    use hbb_common::anyhow::Context;
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tray_icon::{
        menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
        TrayIcon, TrayIconBuilder, TrayIconEvent as TrayEvent,
    };

    // Duplicated tray icons kept piling up through the blind spots of
    // `check_process("--tray", ..)`. https://github.com/rustdesk/rustdesk/issues/15689
    #[cfg(windows)]
    if !crate::platform::windows::try_lock_tray_single_instance() {
        log::info!("Another tray process is already running in this session, exit");
        return Ok(());
    }

    let icon;
    #[cfg(target_os = "macos")]
    {
        icon = include_bytes!("../res/mac-tray-dark-x2.png"); // use as template, so color is not important
    }
    #[cfg(not(target_os = "macos"))]
    {
        icon = include_bytes!("../res/tray-icon.ico");
    }

    let (icon_rgba, icon_width, icon_height) = {
        let image = load_icon_from_asset()
            .unwrap_or(image::load_from_memory(icon).context("Failed to open icon path")?)
            .into_rgba8();
        let (width, height) = image.dimensions();
        let rgba = image.into_raw();
        (rgba, width, height)
    };
    let icon = tray_icon::Icon::from_rgba(icon_rgba, icon_width, icon_height)
        .context("Failed to open icon")?;

    let mut event_loop = EventLoopBuilder::new().build();

    let tray_menu = Menu::new();
    let hide_stop_service = crate::ui_interface::get_builtin_option(
        keys::OPTION_HIDE_STOP_SERVICE,
    ) == "Y";
    // The tray icon is only shown when the service is running, so we don't need to check
    // the `stop-service` option here.
    let quit_i = if !hide_stop_service {
        Some(MenuItem::new(translate("Stop service".to_owned()), true, None))
    } else {
        None
    };
    let open_i = MenuItem::new(translate("Open".to_owned()), true, None);
    let separator_i = PredefinedMenuItem::separator();
    let recent_menu = Submenu::new(translate("Recent sessions".to_owned()), true);
    let initial_recent = recent_peers();
    update_recent_menu(&recent_menu, &initial_recent);
    if let Some(quit_i) = &quit_i {
        tray_menu
            .append_items(&[&recent_menu, &separator_i, &open_i, quit_i])
            .ok();
    } else {
        tray_menu
            .append_items(&[&recent_menu, &separator_i, &open_i])
            .ok();
    }
    let tooltip = |count: usize| {
        if count == 0 {
            format!(
                "{} {}",
                crate::get_app_name(),
                translate("Service is running".to_owned()),
            )
        } else {
            format!(
                "{} - {}\n{}",
                crate::get_app_name(),
                translate("Ready".to_owned()),
                translate("{".to_string() + &format!("{count}") + "} sessions"),
            )
        }
    };
    let mut _tray_icon: Arc<Mutex<Option<TrayIcon>>> = Default::default();

    let menu_channel = MenuEvent::receiver();
    let tray_channel = TrayEvent::receiver();
    #[cfg(windows)]
    let (ipc_sender, ipc_receiver) = std::sync::mpsc::channel::<Data>();

    let open_func = move || {
        if cfg!(not(feature = "flutter")) {
            crate::run_me::<&str>(vec![]).ok();
            return;
        }
        #[cfg(target_os = "macos")]
        crate::platform::macos::handle_application_should_open_untitled_file();
        #[cfg(target_os = "windows")]
        {
            // Do not use "start uni link" way, it may not work on some Windows, and pop out error
            // dialog, I found on one user's desktop, but no idea why, Windows is shit.
            // Use `run_me` instead.
            // `allow_multiple_instances` in `flutter/windows/runner/main.cpp` allows only one instance without args.
            crate::run_me::<&str>(vec![]).ok();
        }
        #[cfg(target_os = "linux")]
        {
            // Do not use "xdg-open", it won't read the config.
            if crate::dbus::invoke_new_connection(crate::get_uri_prefix()).is_err() {
                if let Ok(task) = crate::run_me::<&str>(vec![]) {
                    crate::server::CHILD_PROCESS.lock().unwrap().push(task);
                }
            }
        }
    };

    // Open the session of a recent peer in the main window.
    let open_recent_peer = {
        let open_func = open_func;
        move |id: String| {
            let url = format!("{}connect/{}", crate::get_uri_prefix(), id);
            std::thread::spawn(move || {
                if try_deliver_url(&url) {
                    return;
                }
                // There is no main window yet, open it and hand over the url
                // as soon as it is there.
                open_func();
                for _ in 0..20 {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    if try_deliver_url(&url) {
                        return;
                    }
                }
                log::warn!("Failed to open the recent session from the tray menu");
            });
        }
    };

    // Keep the recent session list up to date, it is read from the peer
    // config files that the main window writes.
    let (recent_sender, recent_receiver) = std::sync::mpsc::channel::<Vec<RecentPeer>>();
    if let Err(err) = std::thread::Builder::new()
        .name("recent-peers-watcher".to_owned())
        .spawn(move || {
            let mut last = initial_recent;
            loop {
                std::thread::sleep(RECENT_REFRESH_INTERVAL);
                let peers = recent_peers();
                if peers != last {
                    last = peers.clone();
                    if recent_sender.send(peers).is_err() {
                        return;
                    }
                }
            }
        })
    {
        log::warn!("Failed to start the recent peers watcher: {err}");
    }

    #[cfg(windows)]
    std::thread::spawn(move || {
        start_query_session_count(ipc_sender.clone());
    });
    #[cfg(windows)]
    let mut last_click = std::time::Instant::now();
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::EventLoopExtMacOS;
        event_loop.set_activation_policy(tao::platform::macos::ActivationPolicy::Accessory);
    }
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(
            std::time::Instant::now() + std::time::Duration::from_millis(100),
        );

        if let tao::event::Event::NewEvents(tao::event::StartCause::Init) = event {
            // for fixing https://github.com/rustdesk/rustdesk/discussions/10210#discussioncomment-14600745
            // so we start tray, but not to show it
            if crate::ui_interface::get_builtin_option(keys::OPTION_HIDE_TRAY) == "Y" {
                return;
            }
            // We create the icon once the event loop is actually running
            // to prevent issues like https://github.com/tauri-apps/tray-icon/issues/90
            let mut builder = TrayIconBuilder::new()
                .with_id(crate::get_app_name().to_lowercase())
                .with_menu(Box::new(tray_menu.clone()))
                .with_tooltip(tooltip(0))
                .with_icon(icon.clone());
            #[cfg(target_os = "macos")]
            {
                builder = builder.with_icon_as_template(true);
            }
            #[cfg(target_os = "windows")]
            {
                // Required since tray-icon 0.17
                // Fixes #15215, #15222, #15410
                builder = builder.with_menu_on_left_click(false);
            }
            let tray = builder.build();
            match tray {
                Ok(tray) => _tray_icon = Arc::new(Mutex::new(Some(tray))),
                Err(err) => {
                    log::error!("Failed to create tray icon: {}", err);
                }
            };

            // We have to request a redraw here to have the icon actually show up.
            // Tao only exposes a redraw method on the Window so we use core-foundation directly.
            #[cfg(target_os = "macos")]
            unsafe {
                use core_foundation::runloop::{CFRunLoopGetMain, CFRunLoopWakeUp};

                let rl = CFRunLoopGetMain();
                CFRunLoopWakeUp(rl);
            }
        }

        if let Ok(peers) = recent_receiver.try_recv() {
            update_recent_menu(&recent_menu, &peers);
        }

        if let Ok(event) = menu_channel.try_recv() {
            if let Some(id) = event.id.0.strip_prefix(RECENT_ITEM_ID_PREFIX) {
                open_recent_peer(id.to_owned());
            } else if let Some(quit_i) = &quit_i {
                if event.id == quit_i.id() {
                    /* failed in windows, seems no permission to check system process
                    if !crate::check_process("--server", false) {
                        *control_flow = ControlFlow::Exit;
                        return;
                    }
                    */
                    // Remove the icon first: on success `uninstall_service()` ends
                    // this process with `std::process::exit`, which skips the
                    // destructor that would remove it, leaving a ghost icon behind.
                    #[cfg(windows)]
                    let _ = _tray_icon
                        .lock()
                        .unwrap()
                        .as_mut()
                        .map(|t| t.set_visible(false));
                    if !crate::platform::uninstall_service(false, false) {
                        *control_flow = ControlFlow::Exit;
                    }
                    // Still alive, so stopping the service failed or was cancelled
                    // in the UAC prompt. Show the icon again.
                    #[cfg(windows)]
                    let _ = _tray_icon
                        .lock()
                        .unwrap()
                        .as_mut()
                        .map(|t| t.set_visible(true));
                } else if event.id == open_i.id() {
                    open_func();
                }
            } else if event.id == open_i.id() {
                open_func();
            }
        }

        if let Ok(_event) = tray_channel.try_recv() {
            #[cfg(target_os = "windows")]
            match _event {
                TrayEvent::Click {
                    button,
                    button_state,
                    ..
                } => {
                    if button == tray_icon::MouseButton::Left
                        && button_state == tray_icon::MouseButtonState::Up
                    {
                        if last_click.elapsed() < std::time::Duration::from_secs(1) {
                            return;
                        }
                        open_func();
                        last_click = std::time::Instant::now();
                    }
                }
                _ => {}
            }
        }

        #[cfg(windows)]
        if let Ok(data) = ipc_receiver.try_recv() {
            match data {
                Data::ControlledSessionCount(count) => {
                    _tray_icon
                        .lock()
                        .unwrap()
                        .as_mut()
                        .map(|t| t.set_tooltip(Some(tooltip(count))));
                }
                _ => {}
            }
        }
    });
}

#[cfg(windows)]
#[tokio::main(flavor = "current_thread")]
async fn start_query_session_count(sender: std::sync::mpsc::Sender<Data>) {
    let mut last_count = 0;
    loop {
        if let Ok(mut c) = crate::ipc::connect(1000, "").await {
            let mut timer = crate::rustdesk_interval(tokio::time::interval(Duration::from_secs(1)));
            loop {
                tokio::select! {
                    res = c.next() => {
                        match res {
                            Err(err) => {
                                log::error!("ipc connection closed: {}", err);
                                break;
                            }

                            Ok(Some(Data::ControlledSessionCount(count))) => {
                                if count != last_count {
                                    last_count = count;
                                    sender.send(Data::ControlledSessionCount(count)).ok();
                                }
                            }
                            _ => {}
                        }
                    }

                    _ = timer.tick() => {
                        c.send(&Data::ControlledSessionCount(0)).await.ok();
                    }
                }
            }
        }
        hbb_common::sleep(1.).await;
    }
}

fn load_icon_from_asset() -> Option<image::DynamicImage> {
    let Some(path) = std::env::current_exe().map_or(None, |x| x.parent().map(|x| x.to_path_buf()))
    else {
        return None;
    };
    #[cfg(target_os = "macos")]
    let path = path.join("../Frameworks/App.framework/Resources/flutter_assets/assets/icon.png");
    #[cfg(windows)]
    let path = path.join(r"data\flutter_assets\assets\icon.png");
    #[cfg(target_os = "linux")]
    let path = path.join(r"data/flutter_assets/assets/icon.png");
    if path.exists() {
        if let Ok(image) = image::open(path) {
            return Some(image);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: &str, label: &str) -> RecentPeer {
        RecentPeer {
            id: id.to_owned(),
            label: label.to_owned(),
        }
    }

    #[test]
    fn format_id_groups_digits_in_three() {
        assert_eq!(format_id("123456789"), "123 456 789");
        assert_eq!(format_id("12345"), "12 345");
        assert_eq!(format_id("123"), "123");
        assert_eq!(format_id("1234"), "1 234");
        assert_eq!(format_id("abc"), "abc");
        assert_eq!(format_id(""), "");
    }

    #[test]
    fn duplicated_labels_get_the_id_appended() {
        let mut peers = vec![
            peer("111111111", "Work PC"),
            peer("222222222", "work pc"),
            peer("333333333", "Laptop"),
        ];
        disambiguate_labels(&mut peers);
        assert_eq!(peers[0].label, "Work PC (111 111 111)");
        assert_eq!(peers[1].label, "work pc (222 222 222)");
        assert_eq!(peers[2].label, "Laptop");
    }

    #[test]
    fn menu_text_escapes_the_mnemonic_and_is_truncated() {
        assert_eq!(sanitize_menu_text("A & B"), "A && B");
        assert_eq!(sanitize_menu_text("a\nb"), "a b");
        let long = "x".repeat(RECENT_LABEL_MAX_LEN + 5);
        let sanitized = sanitize_menu_text(&long);
        assert_eq!(sanitized.chars().count(), RECENT_LABEL_MAX_LEN + 1);
    }

    #[test]
    fn only_url_safe_ids_are_used() {
        assert!(is_url_safe_id("123456"));
        assert!(is_url_safe_id("a-b_c.d"));
        assert!(!is_url_safe_id("123 456"));
        assert!(!is_url_safe_id("a/b"));
        assert!(!is_url_safe_id(""));
    }
}
