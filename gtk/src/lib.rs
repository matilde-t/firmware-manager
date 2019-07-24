#[macro_use]
extern crate cascade;
#[macro_use]
extern crate shrinkwraprs;

mod dialogs;
mod traits;
mod views;

use self::{dialogs::*, views::*};
use firmware_manager::*;

use gtk::{self, prelude::*};
use slotmap::{DefaultKey as Entity, SecondaryMap};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeSet, HashSet},
    error::Error as ErrorTrait,
    iter,
    process::Command,
    rc::Rc,
    sync::{
        mpsc::{channel, Receiver, Sender, TryRecvError},
        Arc,
    },
    thread::{self, JoinHandle},
};

pub struct FirmwareWidget {
    container:  gtk::Container,
    sender:     Sender<FirmwareEvent>,
    background: Option<JoinHandle<()>>,
}

impl FirmwareWidget {
    pub fn new() -> Self {
        #[cfg(all(not(feature = "fwupd"), not(feature = "system76")))]
        compile_error!("must enable one or more of [fwupd system76]");

        let (sender, rx) = channel();
        let (tx, receiver) = glib::MainContext::channel(glib::PRIORITY_DEFAULT);
        let background = Self::background(rx, tx);

        let view_devices = DevicesView::new();
        let view_empty = EmptyView::new();

        let info_bar_label = cascade! {
            gtk::Label::new(None);
            ..set_line_wrap(true);
        };

        let info_bar = cascade! {
            gtk::InfoBar::new();
            ..set_message_type(gtk::MessageType::Error);
            ..set_show_close_button(true);
            // ..set_revealed(false);
            ..set_valign(gtk::Align::Start);
            ..connect_close(|info_bar| {
                // info_bar.set_revealed(false);
                info_bar.set_visible(false);
            });
            ..connect_response(|info_bar, _| {
                // info_bar.set_revealed(false);
                info_bar.set_visible(false);
            });
        };

        if let Some(area) = info_bar.get_content_area() {
            if let Some(area) = area.downcast::<gtk::Container>().ok() {
                area.add(&info_bar_label);
            }
        }

        let stack = cascade! {
            gtk::Stack::new();
            ..add(view_empty.as_ref());
            ..add(view_devices.as_ref());
            ..set_visible_child(view_empty.as_ref());
        };

        let container = cascade! {
            gtk::Overlay::new();
            ..add_overlay(&info_bar);
            ..add(&stack);
            ..show_all();
        };

        info_bar.hide();

        let (tx_progress, rx_progress) = channel();
        progress_handler(rx_progress);

        {
            let sender = sender.clone();
            let stack = stack.clone();

            let mut entities = Entities::default();
            let mut device_widgets: SecondaryMap<Entity, (DeviceWidget, Rc<Cell<bool>>)> = SecondaryMap::new();
            let mut devices_found = false;
            let thelio_io_upgradeable =
                Rc::new(RefCell::new(ThelioData { digest: None, upgradeable: false }));

            receiver.attach(None, move |event| {
                match event {
                    // An event that occurs when firmware has successfully updated.
                    FirmwareSignal::DeviceUpdated(entity, latest) => {
                        let mut device_continue = true;

                        #[cfg(feature = "system76")]
                        {
                            if entities.thelio_io.contains_key(entity) {
                                for entity in entities.thelio_io.keys() {
                                    let widget = &device_widgets[entity].0;
                                    widget.stack.set_visible(false);
                                    widget.label.set_text(latest.as_ref());
                                    let _ = tx_progress
                                        .send(ActivateEvent::Deactivate(widget.progress.clone()));
                                }

                                device_continue = false;
                            }
                        }

                        if device_continue {
                            if let Some((widget, upgradeable)) = device_widgets.get(entity) {
                                widget.stack.set_visible(false);
                                widget.label.set_text(latest.as_ref());
                                upgradeable.set(false);
                                let _ = tx_progress
                                    .send(ActivateEvent::Deactivate(widget.progress.clone()));

                                if entities.is_system(entity) {
                                    reboot();
                                }
                            }
                        }
                    }
                    // An error occurred in the background thread, which we shall display in the UI.
                    FirmwareSignal::Error(entity, why) => {
                        // Convert the error and its causes into a string.
                        let mut error_message = format!("{}", why);
                        let mut cause = why.source();
                        while let Some(error) = cause {
                            error_message.push_str(format!(": {}", error).as_str());
                            cause = error.source();
                        }

                        eprintln!("firmware widget error: {}", error_message);

                        info_bar.set_visible(true);
                        // info_bar.set_revealed(true);
                        info_bar_label.set_text(error_message.as_str().into());

                        if let Some(entity) = entity {
                            let widget = &device_widgets[entity].0;
                            widget.stack.set_visible_child(&widget.button);
                        }
                    }
                    // An event that occurs when fwupd firmware is found.
                    #[cfg(feature = "fwupd")]
                    FirmwareSignal::Fwupd(device, upgradeable, releases) => {
                        devices_found = true;
                        let info = FirmwareInfo {
                            name:    [&device.vendor, " ", &device.name].concat().into(),
                            current: device.version.clone(),
                            latest:  releases.iter().last().expect("no releases").version.clone(),
                        };

                        let entity = entities.insert();

                        let widget = if device.needs_reboot() {
                            entities.associate_system(entity);
                            view_devices.system(&info)
                        } else {
                            view_devices.device(&info)
                        };

                        let data = Rc::new(FwupdDialogData {
                            device: Arc::new(device),
                            releases,
                            entity,
                            shared: DialogData {
                                sender: sender.clone(),
                                tx_progress: tx_progress.clone(),
                                stack: widget.stack.downgrade(),
                                progress: widget.progress.downgrade(),
                                info,
                            },
                        });

                        let upgradeable = Rc::new(Cell::new(upgradeable));

                        if upgradeable.get() {
                            let data = data.clone();
                            let upgradeable = upgradeable.clone();
                            widget
                                .connect_upgrade_clicked(move || {
                                    fwupd_dialog(&data, upgradeable.get(), true)
                                });
                        } else {
                            widget.stack.set_visible(false);
                        }

                        {
                            let upgradeable = upgradeable.clone();
                            widget.connect_clicked(move || {
                                fwupd_dialog(&data, upgradeable.get(), false)
                            });
                        }

                        device_widgets.insert(entity, (widget, upgradeable));
                        stack.show();
                        stack.set_visible_child(view_devices.as_ref());
                    }
                    // Begins searching for devices that have firmware upgrade support
                    FirmwareSignal::Scanning => {
                        view_devices.clear();
                        entities.entities.clear();
                        devices_found = false;

                        let _ = tx_progress.send(ActivateEvent::Clear);

                        stack.hide();
                    }
                    // Signal is received when scanning has completed.
                    FirmwareSignal::ScanningComplete => {
                        if !devices_found {
                            stack.show();
                            stack.set_visible_child(view_empty.as_ref());
                        }
                    }
                    // When system firmwmare is successfully scheduled, reboot the system.
                    FirmwareSignal::SystemScheduled => {
                        reboot();
                    }
                    // An event that occurs when System76 system firmware has been found.
                    #[cfg(feature = "system76")]
                    FirmwareSignal::S76System(info, digest, changelog) => {
                        devices_found = true;
                        let widget = view_devices.system(&info);
                        let entity = entities.insert();
                        entities.associate_system(entity);
                        let upgradeable = info.current != info.latest;

                        let data = Rc::new(System76DialogData {
                            entity,
                            digest,
                            changelog,
                            shared: DialogData {
                                sender: sender.clone(),
                                tx_progress: tx_progress.clone(),
                                stack: widget.stack.downgrade(),
                                progress: widget.progress.downgrade(),
                                info,
                            },
                        });

                        let upgradeable = Rc::new(Cell::new(upgradeable));

                        if upgradeable.get() {
                            let data = data.clone();
                            let upgradeable = upgradeable.clone();
                            widget.connect_upgrade_clicked(move || {
                                s76_system_dialog(&data, upgradeable.get());
                            });
                        } else {
                            widget.stack.set_visible(false);
                        }

                        {
                            let upgradeable = upgradeable.clone();
                            widget.connect_clicked(move || {
                                s76_system_dialog(&data, upgradeable.get());
                            });
                        }

                        device_widgets.insert(entity, (widget, upgradeable));
                        stack.show();
                        stack.set_visible_child(view_devices.as_ref());
                    }
                    // An event that occurs when a Thelio I/O board was discovered.
                    #[cfg(feature = "system76")]
                    FirmwareSignal::ThelioIo(info, digest) => {
                        devices_found = true;
                        let widget = view_devices.device(&info);
                        let entity = entities.insert();
                        let info = Rc::new(info);

                        if info.current != info.latest {
                            thelio_io_upgradeable.borrow_mut().upgradeable = true;
                        }

                        if let Some(digest) = digest {
                            thelio_io_upgradeable.borrow_mut().digest = Some(digest.clone());

                            let sender = sender.clone();
                            let tx_progress = tx_progress.clone();
                            let stack = widget.stack.downgrade();
                            let progress = widget.progress.downgrade();
                            let info = info.clone();

                            widget.connect_upgrade_clicked(move || {
                                // Exchange the button for a progress bar.
                                if let (Some(stack), Some(progress)) =
                                    (stack.upgrade(), progress.upgrade())
                                {
                                    stack.set_visible_child(&progress);
                                    let _ = tx_progress.send(ActivateEvent::Activate(progress));
                                }

                                let _ = sender.send(FirmwareEvent::ThelioIo(
                                    entity,
                                    digest.clone(),
                                    info.latest.clone(),
                                ));
                            });
                        }

                        {
                            let sender = sender.clone();
                            let tx_progress = tx_progress.clone();
                            let stack = widget.stack.downgrade();
                            let progress = widget.progress.downgrade();
                            let upgradeable = thelio_io_upgradeable.clone();
                            let data = thelio_io_upgradeable.clone();
                            let info = info.clone();
                            widget.connect_clicked(move || {
                                let dialog = FirmwareUpdateDialog::new(
                                    info.latest.as_ref(),
                                    iter::once((info.latest.as_ref(), "")),
                                    upgradeable.borrow().upgradeable,
                                    false,
                                );

                                let sender = sender.clone();
                                let tx_progress = tx_progress.clone();

                                if gtk::ResponseType::Accept == dialog.run() {
                                    if let Some(ref digest) = data.borrow().digest {
                                        if let (Some(stack), Some(progress)) =
                                            (stack.upgrade(), progress.upgrade())
                                        {
                                            stack.set_visible_child(&progress);
                                            let _ =
                                                tx_progress.send(ActivateEvent::Activate(progress));
                                        }

                                        let _ = sender.send(FirmwareEvent::ThelioIo(
                                            entity,
                                            digest.clone(),
                                            info.latest.clone(),
                                        ));
                                    }
                                }

                                dialog.destroy();
                            });
                        }

                        widget.stack.set_visible(false);
                        device_widgets.insert(entity, (widget, Rc::new(Cell::new(false))));
                        entities.thelio_io.insert(entity, ());

                        // If any Thelio I/O device requires an update, then enable the
                        // update button on the first Thelio I/O device widget.
                        if thelio_io_upgradeable.borrow_mut().upgradeable {
                            let entity = entities
                                .thelio_io
                                .keys()
                                .next()
                                .expect("missing thelio I/O widgets");
                            device_widgets[entity].0.stack.set_visible(true);
                        }

                        stack.show();
                        stack.set_visible_child(view_devices.as_ref());
                    }
                    // This is the last message sent before the background thread exits.
                    FirmwareSignal::Stop => {
                        return glib::Continue(false);
                    }
                }

                glib::Continue(true)
            });
        }

        Self {
            background: Some(background),
            container: container.upcast::<gtk::Container>(),
            sender,
        }
    }

    pub fn scan(&self) { let _ = self.sender.send(FirmwareEvent::Scan); }

    pub fn container(&self) -> &gtk::Container { self.container.upcast_ref::<gtk::Container>() }

    /// Manages all firmware client interactions from a background thread.
    fn background(
        receiver: Receiver<FirmwareEvent>,
        sender: glib::Sender<FirmwareSignal>,
    ) -> JoinHandle<()> {
        thread::spawn(move || {
            firmware_manager::event_loop(receiver, |event| {
                let _ = sender.send(event);
            });

            let _ = sender.send(FirmwareSignal::Stop);

            eprintln!("stopping firmware client connection");
        })
    }
}

impl Drop for FirmwareWidget {
    fn drop(&mut self) {
        let _ = self.sender.send(FirmwareEvent::Quit);

        if let Some(handle) = self.background.take() {
            let _ = handle.join();
        }
    }
}

fn reboot() {
    if let Err(why) = Command::new("systemctl").arg("reboot").status() {
        eprintln!("failed to reboot: {}", why);
    }
}

/// Senders and widgets shared by all device dialogs.
struct DialogData {
    sender:      Sender<FirmwareEvent>,
    tx_progress: Sender<ActivateEvent>,
    stack:       glib::WeakRef<gtk::Stack>,
    progress:    glib::WeakRef<gtk::ProgressBar>,
    info:        FirmwareInfo,
}

#[cfg(feature = "fwupd")]
struct FwupdDialogData {
    entity:   Entity,
    device:   Arc<FwupdDevice>,
    releases: BTreeSet<FwupdRelease>,
    shared:   DialogData,
}

fn fwupd_dialog(data: &FwupdDialogData, upgradeable: bool, upgrade_button: bool) {
    let &FwupdDialogData { entity, device, releases, shared } = &data;
    let &DialogData { sender, tx_progress, stack, progress, info } = &shared;

    let response = if !upgrade_button || device.needs_reboot() {
        let &FirmwareInfo { ref latest, .. } = &info;

        let log_entries =
            releases.iter().rev().map(|release| (release.version.as_ref(), release.description.as_ref()));

        let dialog = FirmwareUpdateDialog::new(latest, log_entries, upgradeable, device.needs_reboot());

        let response = dialog.run();
        dialog.destroy();
        response
    } else {
        gtk::ResponseType::Accept.into()
    };

    if gtk::ResponseType::Accept == response {
        // Exchange the button for a progress bar.
        if let (Some(stack), Some(progress)) = (stack.upgrade(), progress.upgrade()) {
            stack.set_visible_child(&progress);
            let _ = tx_progress.send(ActivateEvent::Activate(progress));
        }

        let _ = sender.send(FirmwareEvent::Fwupd(
            *entity,
            device.clone(),
            Arc::new(releases.iter().last().expect("no release found").clone()),
        ));
    }
}

#[cfg(feature = "system76")]
struct System76DialogData {
    entity:    Entity,
    digest:    System76Digest,
    changelog: System76Changelog,
    shared:    DialogData,
}

#[cfg(feature = "system76")]
fn s76_system_dialog(data: &System76DialogData, upgradeable: bool) {
    let &System76DialogData { entity, digest, changelog, shared } = &data;
    let &DialogData { sender, tx_progress, stack, progress, info } = &shared;
    let &FirmwareInfo { latest, .. } = &info;

    let log_entries = changelog
        .versions
        .iter()
        .map(|version| (version.bios.as_ref(), version.description.as_ref()));

    let dialog = FirmwareUpdateDialog::new(latest, log_entries, upgradeable, true);

    if gtk::ResponseType::Accept == dialog.run() {
        // Exchange the button for a progress bar.
        if let (Some(stack), Some(progress)) = (stack.upgrade(), progress.upgrade()) {
            stack.set_visible_child(&progress);
            let _ = tx_progress.send(ActivateEvent::Activate(progress));
        }

        let event = FirmwareEvent::S76System(*entity, digest.clone(), latest.clone());
        let _ = sender.send(event);
    }

    dialog.destroy();
}

/// Activates, or deactivates, the movement of progress bars.
/// TODO: As soon as glib::WeakRef supports Eq/Hash derives, use WeakRef instead.
enum ActivateEvent {
    Activate(gtk::ProgressBar),
    Deactivate(gtk::ProgressBar),
    Clear,
}

fn progress_handler(rx_progress: Receiver<ActivateEvent>) {
    let mut active_widgets: HashSet<gtk::ProgressBar> = HashSet::new();
    let mut remove = Vec::new();
    gtk::timeout_add(100, move || {
        loop {
            match rx_progress.try_recv() {
                Ok(ActivateEvent::Activate(widget)) => {
                    active_widgets.insert(widget);
                }
                Ok(ActivateEvent::Deactivate(widget)) => {
                    active_widgets.remove(&widget);
                }
                Ok(ActivateEvent::Clear) => {
                    active_widgets.clear();
                    return gtk::Continue(true);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    return gtk::Continue(false);
                }
            }
        }

        for widget in remove.drain(..) {
            active_widgets.remove(&widget);
        }

        for widget in &active_widgets {
            widget.pulse();
        }

        gtk::Continue(true)
    });
}

struct ThelioData {
    digest:      Option<System76Digest>,
    upgradeable: bool,
}
