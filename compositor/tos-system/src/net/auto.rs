//! What a machine with nobody sitting at it does about its links.
//!
//! [#124](https://github.com/m96-chan/tOS/issues/124): a tOS machine with
//! `openssh-server` on it has `sshd` listening three seconds into the boot and
//! no address for anyone to reach it at, because the only thing in the tree
//! that brings a link up is a menu, and a menu needs somebody at the keyboard.
//! A machine that has to be logged into at the console before it can be
//! reached over the network is not a machine anything can be served from.
//!
//! `docs/design/network.md` already called this the wired requirement — "DHCP
//! on link-up is enough to start" — so what is here is that sentence and
//! nothing beyond it: the links a person would have brought up by hand get
//! brought up by themselves, and the address they would then have asked for
//! gets asked for.
//!
//! This is the policy and none of the mechanism. It is handed the interfaces
//! as they were last read and says which one to do what to; the compositor is
//! what calls [`Network::bring_up`](super::Network::bring_up) and what starts
//! the DHCP client. The split is what makes "a second link is not asked while
//! the first one is still asking" a table test rather than a machine with two
//! cards in it.
//!
//! Four rules, and it does nothing else:
//!
//! * **A radio is never brought up and never scanned by this.** The
//!   supplicant owns the link's administrative state and does its own
//!   scanning; a radio with no supplicant on it is a radio nothing here can do
//!   anything with. **A radio that is associated asks for an address**, once
//!   per association, exactly as a cable asks once per carrier — because
//!   association *is* carrier on a wireless link: `/sys/class/net/wlan0/carrier`
//!   reads `1` the moment the four-way handshake completes and `0` when it
//!   drops. Loopback and virtual devices are nobody's idea of "the network"
//!   and are left out of all of it.
//! * **Never over an address.** A link that already has a routable address is
//!   left exactly as it is, whoever gave it one — the menu, a previous lease,
//!   or a static address somebody wrote in by hand.
//! * **Once per carrier.** An address is asked for once per carrier, so a server
//!   that does not answer costs one DHCP conversation rather than one every
//!   second forever. Pulling the cable out and putting it back is a new
//!   carrier and a new ask, which is also what a person means by it.
//! * **One at a time.** There is one DHCP client and it runs on one interface,
//!   so a machine with two cards in it gets them in the order the kernel lists
//!   them rather than a race between two clients for one `/etc/resolv.conf`.

use std::collections::HashMap;

use super::{Address, Interface, Kind};

/// One thing to do to one link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Set `IFF_UP`: [`Network::bring_up`](super::Network::bring_up).
    BringUp(String),
    /// Start a DHCP acquisition, on a link that is up, carrying, and has
    /// nothing to show for it.
    Ask(String),
}

impl Step {
    /// The interface the step is about.
    pub fn interface(&self) -> &str {
        match self {
            Step::BringUp(interface) | Step::Ask(interface) => interface,
        }
    }
}

/// What has already been tried on a link, so that it is not tried again.
#[derive(Debug, Default, Clone, Copy)]
struct Attempts {
    /// It has been asked to come up. Once per link per session: an
    /// `SIOCSIFFLAGS` the kernel refused is one it will refuse a second later
    /// for the same reason, and an ioctl repeated once a second is a log full
    /// of the same denial.
    brought_up: bool,
    /// It has been asked for an address while this carrier has been up.
    /// Cleared when the carrier goes, which is what makes a replug a new ask.
    asked: bool,
}

/// The links, brought up and addressed without anybody asking.
///
/// Keeps only what it has already done. Everything else it needs is in the
/// interfaces it is handed, so a machine whose cards changed underneath it —
/// a USB adapter pulled out, a card renamed — is described by the next read
/// rather than by anything remembered here.
#[derive(Debug, Default)]
pub struct Autoconfigure {
    attempts: HashMap<String, Attempts>,
}

impl Autoconfigure {
    pub fn new() -> Autoconfigure {
        Autoconfigure::default()
    }

    /// The next thing to do, or `None` when there is nothing to be done.
    ///
    /// `asking` is whether a DHCP acquisition is already running; it holds
    /// back [`Step::Ask`] and nothing else, because bringing a second link up
    /// while the first one is talking to a server costs nothing and is what
    /// gets that link carrying by the time the client is free.
    ///
    /// Every interface is looked at even once the answer is known, because the
    /// bookkeeping is per link: a cable pulled out of the second card has to
    /// be noticed on the pass where the first card is what is being acted on.
    pub fn next(&mut self, interfaces: &[Interface], asking: bool) -> Option<Step> {
        // A card that is not there any more is forgotten rather than kept
        // against the day its name comes back. A USB adapter pulled out and
        // put back in is a link that has been through nothing, and the thing
        // a person expects of it is what a card that was always there gets.
        self.attempts
            .retain(|name, _| interfaces.iter().any(|interface| &interface.name == name));

        let mut step = None;
        for interface in interfaces {
            if interface.kind != Kind::Wired && interface.kind != Kind::Wireless {
                continue;
            }
            let attempts = self.attempts.entry(interface.name.clone()).or_default();
            if !interface.carrier {
                attempts.asked = false;
            }
            if interface.addresses.iter().any(Address::is_routable) {
                continue;
            }
            if step.is_some() {
                continue;
            }
            if !interface.admin_up {
                // A radio that is down is left down. `IFF_UP` on a wireless
                // link is the supplicant's to set — it needs the interface up
                // to scan and to associate, and it puts it back down when it
                // stops — so a radio brought up from here is a radio taken
                // away from whoever is driving it, with nothing to associate
                // with to show for it.
                if interface.kind == Kind::Wired && !attempts.brought_up {
                    attempts.brought_up = true;
                    step = Some(Step::BringUp(interface.name.clone()));
                }
                continue;
            }
            // Up but with no cable in it — or a radio associated with nothing:
            // there is nothing to ask, and asking anyway would spend the one
            // ask this carrier gets on a DISCOVER that goes nowhere.
            if interface.carrier && !attempts.asked && !asking {
                attempts.asked = true;
                step = Some(Step::Ask(interface.name.clone()));
            }
        }
        step
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{LinkState, Wireless};

    /// A wired card that is switched off, which is how every interface on a
    /// machine that has just booted looks.
    fn wired(name: &str) -> Interface {
        Interface {
            name: name.to_string(),
            kind: Kind::Wired,
            state: LinkState::Down,
            carrier: false,
            admin_up: false,
            mac: Some("aa:bb:cc:dd:ee:01".to_string()),
            mtu: Some(1500),
            speed_mbps: None,
            addresses: Vec::new(),
            is_default: false,
            gateway: None,
            rx_bytes: 0,
            tx_bytes: 0,
            wireless: None,
        }
    }

    /// The same card after `IFF_UP` and a cable.
    fn carrying(name: &str) -> Interface {
        Interface {
            state: LinkState::Up,
            carrier: true,
            admin_up: true,
            ..wired(name)
        }
    }

    /// A radio with no supplicant on it yet: switched off, associated with
    /// nothing.
    fn radio(name: &str) -> Interface {
        Interface {
            kind: Kind::Wireless,
            wireless: Some(Wireless::default()),
            ..wired(name)
        }
    }

    /// The same radio once the supplicant has brought it up and the four-way
    /// handshake has completed, which is what `carrier` reads as `1` means on
    /// a wireless link.
    fn associated(name: &str) -> Interface {
        Interface {
            state: LinkState::Up,
            carrier: true,
            admin_up: true,
            ..radio(name)
        }
    }

    fn with_address(interface: Interface, address: &str) -> Interface {
        Interface {
            addresses: vec![Address::parse(address).expect("an address")],
            ..interface
        }
    }

    #[test]
    fn a_wired_link_that_is_down_is_brought_up() {
        let mut auto = Autoconfigure::new();
        assert_eq!(
            auto.next(&[wired("eth0")], false),
            Some(Step::BringUp("eth0".to_string()))
        );
    }

    #[test]
    fn a_link_the_kernel_would_not_bring_up_is_not_asked_again() {
        // The ioctl failed, so the interface is still down on the next look.
        // Asking once a second for the rest of the session would fill the
        // journal with one denial per second and change nothing.
        let mut auto = Autoconfigure::new();
        assert!(auto.next(&[wired("eth0")], false).is_some());
        assert_eq!(auto.next(&[wired("eth0")], false), None);
    }

    #[test]
    fn a_link_that_is_up_and_carrying_is_asked_for_an_address() {
        let mut auto = Autoconfigure::new();
        assert_eq!(
            auto.next(&[carrying("eth0")], false),
            Some(Step::Ask("eth0".to_string()))
        );
        // And once: a server that did not answer is not asked every second.
        assert_eq!(auto.next(&[carrying("eth0")], false), None);
    }

    #[test]
    fn a_link_that_is_up_with_no_cable_in_it_is_left_alone() {
        let mut auto = Autoconfigure::new();
        let unplugged = Interface {
            admin_up: true,
            ..wired("eth0")
        };
        assert_eq!(auto.next(&[unplugged], false), None);
    }

    #[test]
    fn a_link_that_already_has_an_address_is_not_touched() {
        let mut auto = Autoconfigure::new();
        let configured = with_address(carrying("eth0"), "192.168.1.5/24");
        assert_eq!(auto.next(&[configured], false), None);
    }

    #[test]
    fn a_link_with_nothing_but_a_link_local_address_is_still_asked() {
        // `169.254/16` is what a link gives itself when nothing gave it
        // anything, so it is the state this exists to get a machine out of.
        let mut auto = Autoconfigure::new();
        let stranded = with_address(carrying("eth0"), "169.254.7.7/16");
        assert_eq!(
            auto.next(&[stranded], false),
            Some(Step::Ask("eth0".to_string()))
        );
    }

    #[test]
    fn nothing_that_is_not_a_cable_or_a_radio_is_touched() {
        let mut auto = Autoconfigure::new();
        let others = [
            Interface {
                kind: Kind::Loopback,
                ..wired("lo")
            },
            Interface {
                kind: Kind::Virtual,
                ..wired("docker0")
            },
        ];
        assert_eq!(auto.next(&others, false), None);
    }

    #[test]
    fn an_associated_radio_with_no_address_is_asked_for_an_address() {
        // Association is carrier on a wireless link, and a link that is
        // carrying with nothing to show for it is the case this exists for,
        // whether what it is carrying arrived on a cable or over the air.
        let mut auto = Autoconfigure::new();
        assert_eq!(
            auto.next(&[associated("wlan0")], false),
            Some(Step::Ask("wlan0".to_string()))
        );
        assert_eq!(auto.next(&[associated("wlan0")], false), None);
    }

    #[test]
    fn a_radio_that_is_down_is_not_brought_up() {
        // The supplicant owns `IFF_UP` on a radio: it needs the link up to
        // scan and puts it back down when it stops. A radio brought up from
        // here is one taken off whoever is driving it, with nothing to
        // associate with to show for it.
        let mut auto = Autoconfigure::new();
        assert_eq!(auto.next(&[radio("wlan0")], false), None);
    }

    #[test]
    fn a_radio_that_is_up_and_associated_with_nothing_is_not_asked() {
        let mut auto = Autoconfigure::new();
        let scanning = Interface {
            admin_up: true,
            ..radio("wlan0")
        };
        assert_eq!(auto.next(&[scanning], false), None);
    }

    #[test]
    fn a_radio_that_associated_again_is_asked_again() {
        // The supplicant lost the network and found it again; that is a new
        // association, and a new association is a new lease to ask for.
        let mut auto = Autoconfigure::new();
        assert!(auto.next(&[associated("wlan0")], false).is_some());

        let dropped = Interface {
            admin_up: true,
            ..radio("wlan0")
        };
        assert_eq!(auto.next(&[dropped], false), None);

        assert_eq!(
            auto.next(&[associated("wlan0")], false),
            Some(Step::Ask("wlan0".to_string()))
        );
    }

    #[test]
    fn a_radio_that_already_has_an_address_is_not_touched() {
        let mut auto = Autoconfigure::new();
        let configured = with_address(associated("wlan0"), "192.168.1.5/24");
        assert_eq!(auto.next(&[configured], false), None);
    }

    #[test]
    fn a_cable_and_a_radio_that_both_carry_are_asked_one_at_a_time() {
        // There is still one DHCP client. The kernel's order decides which
        // goes first, and the other gets the pass after the first one is
        // finished rather than a race for one `/etc/resolv.conf`.
        let mut auto = Autoconfigure::new();
        let links = [carrying("eth0"), associated("wlan0")];
        assert_eq!(
            auto.next(&links, false),
            Some(Step::Ask("eth0".to_string()))
        );
        assert_eq!(auto.next(&links, true), None);
        assert_eq!(
            auto.next(&links, false),
            Some(Step::Ask("wlan0".to_string()))
        );
    }

    #[test]
    fn a_second_link_waits_for_the_one_that_is_already_asking() {
        let mut auto = Autoconfigure::new();
        let links = [carrying("eth0"), carrying("eth1")];
        assert_eq!(
            auto.next(&links, false),
            Some(Step::Ask("eth0".to_string()))
        );
        // eth0's client is running now, so eth1 gets nothing yet — and when it
        // does, it is not eth0 a second time.
        assert_eq!(auto.next(&links, true), None);
        assert_eq!(
            auto.next(&links, false),
            Some(Step::Ask("eth1".to_string()))
        );
    }

    #[test]
    fn a_second_link_is_still_brought_up_while_the_first_one_asks() {
        let mut auto = Autoconfigure::new();
        let links = [carrying("eth0"), wired("eth1")];
        assert_eq!(
            auto.next(&links, false),
            Some(Step::Ask("eth0".to_string()))
        );
        assert_eq!(
            auto.next(&links, true),
            Some(Step::BringUp("eth1".to_string()))
        );
    }

    #[test]
    fn a_cable_that_came_out_and_went_back_in_is_asked_again() {
        let mut auto = Autoconfigure::new();
        assert!(auto.next(&[carrying("eth0")], false).is_some());

        // Unplugged: up, no carrier, and whatever the lease gave it is gone.
        let unplugged = Interface {
            admin_up: true,
            ..wired("eth0")
        };
        assert_eq!(auto.next(&[unplugged], false), None);

        assert_eq!(
            auto.next(&[carrying("eth0")], false),
            Some(Step::Ask("eth0".to_string()))
        );
    }

    #[test]
    fn a_link_that_went_away_and_came_back_is_a_new_link() {
        // A USB adapter pulled out and put back is a link that has been
        // through nothing, so it gets what a card that was always there gets:
        // it is down, so it comes up. Without the adapter having gone, the
        // refusal above stands and it does not.
        let mut auto = Autoconfigure::new();
        assert!(auto.next(&[wired("eth0")], false).is_some());
        assert_eq!(auto.next(&[wired("eth0")], false), None);

        assert_eq!(auto.next(&[], false), None);
        assert_eq!(
            auto.next(&[wired("eth0")], false),
            Some(Step::BringUp("eth0".to_string()))
        );
    }
}
