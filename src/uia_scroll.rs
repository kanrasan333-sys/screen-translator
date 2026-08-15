//! Scrolling a document through UI Automation instead of the mouse wheel.
//!
//! The wheel is a blunt instrument: it goes to whatever window happens to be
//! under the pointer, moves it by an amount the app decides, and gives no way
//! to ask "where am I now" or "is there more".  A `ScrollPattern` answers all
//! three — it exposes the scroll position as a percentage, the share of the
//! document currently on screen, and a setter that moves straight to a target.
//!
//! Chromium (so Chrome, Edge and Electron), Firefox, WPF, WinForms, UWP and
//! Explorer all expose it.  Apps that paint their own scrolling without
//! publishing the pattern — some PDF viewers, canvas editors, games — don't,
//! which is why the wheel path stays as the fallback.

use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::POINT;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationScrollPattern,
    UIA_ScrollPatternId, UIA_ScrollPatternNoScroll,
};

/// Scroll percentages are doubles, but apps round them; anything this close to
/// the end counts as the end.
const END_EPS: f64 = 0.05;

/// How long to wait for a scroll to come to rest before giving up on it.
const SETTLE_TIMEOUT: Duration = Duration::from_millis(1200);
const POLL_MS: u64 = 30;

/// Rendering trails the scroll model slightly — this covers the gap once the
/// position itself has stopped changing.
const PAINT_MS: u64 = 90;

/// How far up the ancestor chain to look for something scrollable.  The
/// element under the pointer is usually a leaf (a line of text, an image)
/// several levels below the scroll container.
const MAX_ANCESTORS: usize = 16;

/// How many times to go looking, and how long to let a lazily-built
/// accessibility tree come up between tries.
const UIA_ATTEMPTS: usize = 3;
const WAKE_DELAY: Duration = Duration::from_millis(400);

pub struct Scroller {
    pattern: IUIAutomationScrollPattern,
    /// Percentage of the scroll range covered by one step.
    step_percent: f64,
    /// Pixels that step is expected to move the content.
    step_px: i32,
    /// Where the user had the document before we touched it.
    start_percent: f64,
}

impl Scroller {
    /// Finds the scrollable element under `(x, y)` and sizes a step that moves
    /// the content about `target_px` pixels.  `None` means this app doesn't
    /// expose a usable scroll pattern — fall back to the wheel.
    pub fn at_point(x: i32, y: i32, target_px: i32) -> Option<Self> {
        unsafe {
            let automation: IUIAutomation =
                CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()?;

            // Chromium — so Chrome, Edge and every Electron app — builds its
            // accessibility tree only once a client asks for it.  The first
            // query is the ask; it comes back with a stub that has no patterns
            // on it.  Giving the tree a moment and looking again is the
            // difference between driving those apps properly and falling back
            // to the wheel for all of them.
            for attempt in 0..UIA_ATTEMPTS {
                if attempt > 0 {
                    thread::sleep(WAKE_DELAY);
                }
                if let Some(s) = Self::search(&automation, x, y, target_px) {
                    return Some(s);
                }
            }
            None
        }
    }

    unsafe fn search(
        automation: &IUIAutomation,
        x: i32,
        y: i32,
        target_px: i32,
    ) -> Option<Self> {
        unsafe {
            let walker = automation.ControlViewWalker().ok()?;
            let mut element = automation.ElementFromPoint(POINT { x, y }).ok()?;
            for _ in 0..MAX_ANCESTORS {
                if let Some(s) = Self::from_element(&element, target_px) {
                    return Some(s);
                }
                element = walker.GetParentElement(&element).ok()?;
            }
            None
        }
    }

    unsafe fn from_element(element: &IUIAutomationElement, target_px: i32) -> Option<Self> {
        unsafe {
            let pattern: IUIAutomationScrollPattern = element
                .GetCurrentPatternAs(UIA_ScrollPatternId)
                .ok()?;

            if !pattern.CurrentVerticallyScrollable().ok()?.as_bool() {
                return None;
            }

            // Share of the document on screen, as a percentage.  At 100 there
            // is nothing below the fold and no reason to be here.
            let view = pattern.CurrentVerticalViewSize().ok()?;
            if !(0.0..99.5).contains(&view) {
                return None;
            }

            let start_percent = pattern.CurrentVerticalScrollPercent().ok()?;
            if start_percent == UIA_ScrollPatternNoScroll {
                return None;
            }

            // The pattern speaks in percentages, the stitcher in pixels.  The
            // element's own height plus the view size give the exchange rate:
            // if a fifth of the document is visible, the four fifths below it
            // are the distance 0-100 % covers.
            let rect = element.CurrentBoundingRectangle().ok()?;
            let viewport_px = (rect.bottom - rect.top) as f64;
            if viewport_px < 1.0 {
                return None;
            }
            let range_px = viewport_px * (100.0 - view) / view;
            if range_px < 1.0 {
                return None;
            }

            let step_percent = (target_px as f64 * 100.0 / range_px).clamp(0.01, 100.0);
            println!(
                "[uia] element class={:?} name={:?} view={view:.2}% at={start_percent:.2}% \
                 range={range_px:.0}px step={step_percent:.3}%",
                element.CurrentClassName().map(|s| s.to_string()).ok(),
                element.CurrentName().map(|s| s.to_string()).ok(),
            );
            Some(Self {
                pattern,
                step_percent,
                step_px: target_px,
                start_percent,
            })
        }
    }

    /// Pixels the next step should move the content — a precise hint for the
    /// frame matcher, where the wheel path has to measure it by trial.
    pub fn step_px(&self) -> i32 {
        self.step_px
    }

    /// Advances one step and waits for the scroll to come to rest.  `false`
    /// means the document was already at the bottom.
    pub fn step(&self) -> bool {
        unsafe {
            let Ok(current) = self.pattern.CurrentVerticalScrollPercent() else {
                return false;
            };
            if current >= 100.0 - END_EPS {
                return false;
            }

            let target = (current + self.step_percent).min(100.0);
            if self
                .pattern
                .SetScrollPercent(UIA_ScrollPatternNoScroll, target)
                .is_err()
            {
                return false;
            }

            self.settle();
            println!(
                "[uia] {current:.2}% -> asked {target:.2}%, landed {:.2}%",
                self.pattern.CurrentVerticalScrollPercent().unwrap_or(-1.0)
            );
            true
        }
    }

    /// Polls the position until it stops changing.  Smooth-scroll animations
    /// vary wildly in length between apps, and this replaces guessing at a
    /// fixed delay long enough to cover the slowest of them.
    unsafe fn settle(&self) {
        unsafe {
            let deadline = Instant::now() + SETTLE_TIMEOUT;
            let mut last = f64::NAN;
            while Instant::now() < deadline {
                thread::sleep(Duration::from_millis(POLL_MS));
                let Ok(now) = self.pattern.CurrentVerticalScrollPercent() else {
                    break;
                };
                if now == last {
                    break;
                }
                last = now;
            }
            thread::sleep(Duration::from_millis(PAINT_MS));
        }
    }

    /// Puts the document back where the user left it.
    pub fn restore(&self) {
        unsafe {
            let _ = self
                .pattern
                .SetScrollPercent(UIA_ScrollPatternNoScroll, self.start_percent);
        }
    }
}
