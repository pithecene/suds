//! The keys of the window's own buttons, sent without a display.

use slint::platform::WindowEvent;
use slint::{ComponentHandle, SharedString};

slint::slint! {
    #[edition = "1.19"]
    import { Check, GroupHead, IconButton, Push } from "ui/controls.slint";

    export component Probe inherits Window {
        in property <int> focused;
        out property <int> presses;
        push := Push { text: "Remove"; clicked => { root.presses += 1; } }
        icon := IconButton { label: "Measure again"; clicked => { root.presses += 1; } }
        check := Check { label: "Remove it"; toggled => { root.presses += 1; } }
        head := GroupHead { name: "go"; toggled => { root.presses += 1; } }
        public function aim() {
            if (root.focused == 0) {
                push.focus();
            } else if (root.focused == 1) {
                icon.focus();
            } else if (root.focused == 2) {
                check.focus();
            } else {
                head.focus();
            }
        }
    }
}

fn probe(focused: i32) -> Probe {
    let probe = Probe::new().unwrap();
    probe.set_focused(focused);
    probe.invoke_aim();
    probe
}

fn send(probe: &Probe, event: WindowEvent) {
    probe.window().dispatch_event(event);
}

#[test]
fn a_held_key_presses_a_button_once() {
    i_slint_backend_testing::init_no_event_loop();
    // The last component is a group's line in the stores' pane, which the Tab key must reach.
    for focused in 0..4 {
        for key in [" ", "\n"] {
            let probe = probe(focused);
            let text = SharedString::from(key);
            send(&probe, WindowEvent::KeyPressed { text: text.clone() });
            assert_eq!(probe.get_presses(), 1, "component {focused}");
            // The removal's button asks once more on its first press, so a repeat must press nothing.
            for _ in 0..3 {
                send(&probe, WindowEvent::KeyPressRepeated { text: text.clone() });
            }
            assert_eq!(probe.get_presses(), 1, "component {focused}");
            send(&probe, WindowEvent::KeyReleased { text: text.clone() });
            send(&probe, WindowEvent::KeyPressed { text });
            assert_eq!(probe.get_presses(), 2, "component {focused}");
        }
    }
}
