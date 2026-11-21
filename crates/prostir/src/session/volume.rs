//! The shared volume: the headset's and the PC's, kept as one.

use super::*;

/// Does what the volume logic says: asks the headset (through `outbox`) or puts the
/// device at the headset's level.
fn do_volume(sink: &SinkVolume, act: Option<Act>, outbox: &outbox::Outbox) {
    match act {
        Some(Act::Ask(asked)) => {
            // At most every 50 ms while a slider moves.
            info!("volume: asking the headset for {}", asked.level);
            outbox.control(HostMsg::VolumeAsk { asked });
        }
        Some(Act::Write { level, muted }) => sink.set(level, muted),
        None => {}
    }
}

impl Session {
    /// The headset is asked to take the level the PC's device has now, where the PC
    /// knows it (the device does not move).
    pub(super) fn take_pc_volume(&mut self) {
        let Some((sink, pc)) = &mut self.volume else {
            return;
        };
        if let Some((level, muted)) = sink.level() {
            let act = pc.device(level, muted, Instant::now());
            do_volume(sink, act, &self.outbox);
        }
    }

    /// A volume from the headset (in `Open` or since): the main headset's goes to the
    /// device. The device's own changes come first: they come through the same channel
    /// of events (see [`Session::device_volume`]), so one that came before this level
    /// makes it an old one, which must not be written while the slider moves.
    pub(super) fn volume_from_headset(&mut self, v: state::Volume) {
        if !self.standing.main {
            return;
        }
        if let Some((sink, pc)) = &mut self.volume {
            let act = pc.from_headset(&v, Instant::now());
            do_volume(sink, act, &self.outbox);
        }
    }

    /// The sound system said the device is at `level` (moved at `at`, read after the
    /// PC's `writes`th write of it): a move asks the headset, the PC's own write is its
    /// echo, and a reading from before the PC's last write is old news. A device made
    /// anew (`new_device`) is at the sound system's level, not the person's: the
    /// headset owns the level, which goes on it.
    pub fn device_volume(
        &mut self,
        level: u8,
        muted: bool,
        at: Instant,
        writes: u64,
        new_device: bool,
    ) {
        let Some((sink, pc)) = &mut self.volume else {
            return;
        };
        if sink.outdated(writes) {
            if new_device {
                info!("volume: a new device for the headset, read before the PC set it");
            }
            return;
        }
        if new_device {
            if let Some(v) = self.publisher.headset.volume {
                info!(
                    "volume: a new device for the headset at {level}: it takes the headset's {}",
                    v.level
                );
                let act = pc.from_headset(&v, Instant::now());
                do_volume(sink, act, &self.outbox);
            }
            return;
        }
        let act = pc.device(level, muted, at);
        do_volume(sink, act, &self.outbox);
    }

    /// The PC's settings window sets the volume: the device goes to `level` (its mute
    /// as it is) and the headset is asked for it, as when the device's own slider
    /// moves. False where the volume is not shared with this headset.
    pub fn volume_from_pc(&mut self, level: u8) -> bool {
        if self.shared_volume().is_none() {
            return false;
        }
        let Some((sink, pc)) = &mut self.volume else {
            return false;
        };
        let muted = sink
            .level()
            .or_else(|| self.publisher.headset.volume.map(|v| (v.level, v.muted)))
            .is_some_and(|(_, muted)| muted);
        info!("volume: {level} from the PC's settings");
        sink.set(level.min(100), muted);
        let act = pc.set_here(level, muted, Instant::now());
        do_volume(sink, act, &self.outbox);
        true
    }

    /// Follows the device's volume: a move waiting asks the headset, its answers go to
    /// the device once the slider rests.
    pub fn volume_due(&mut self) {
        let Some((sink, pc)) = &mut self.volume else {
            return;
        };
        let act = pc.tick(Instant::now());
        do_volume(sink, act, &self.outbox);
    }

    /// When the volume has something to do next.
    pub fn volume_at(&self) -> Option<Instant> {
        self.volume.as_ref().and_then(|(_, pc)| pc.due())
    }

    /// Looks again which monitor the sound comes from, while the headset wants to know.
    pub fn look_at_sound(&mut self, displays: &Displays) {
        if let Some(w) = &self.sound_source {
            let id = w.output().and_then(|key| displays.id_of_output(&key));
            if self.sound_from != id {
                info!("sound comes from {id:?}");
            }
            self.sound_from = id;
        }
    }
}
