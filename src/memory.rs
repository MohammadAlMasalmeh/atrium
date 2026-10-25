//! Conservative renderer eviction policy, independent of the browser engine.
use std::time::Duration;
#[derive(Clone, Debug)]
pub struct Candidate {
    pub id: u64,
    pub idle: Duration,
    pub visible: bool,
    pub loading: bool,
    pub protected: bool,
    /// Frames in the page, counting subframes: a cheap stand-in for its memory cost,
    /// since each cross-site frame gets its own renderer process.
    pub frames: usize,
}
#[derive(Clone, Debug)]
pub struct Policy {
    pub max_live_tabs: usize,
    pub idle_after: Duration,
    pub minimum_idle: Duration,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            max_live_tabs: 6,
            idle_after: Duration::from_secs(300),
            minimum_idle: Duration::from_secs(30),
        }
    }
}
impl Policy {
    /// When the system is short of memory, keep fewer background pages and release them sooner.
    pub fn under_pressure(&self) -> Self {
        Self {
            max_live_tabs: self.max_live_tabs.min(2),
            idle_after: self.idle_after.min(Duration::from_secs(60)),
            minimum_idle: self.minimum_idle,
        }
    }
    pub fn candidates(&self, tabs: &[Candidate]) -> Vec<u64> {
        let mut eligible: Vec<_> = tabs
            .iter()
            .filter(|t| !t.visible && !t.loading && !t.protected && t.idle >= self.minimum_idle)
            .collect();
        // Over budget, the heaviest pages go first. Ad-heavy pages hold dozens of frames
        // and keep spawning more while hidden; evicting a light page saves little.
        eligible.sort_by_key(|t| (std::cmp::Reverse(t.frames), std::cmp::Reverse(t.idle)));
        let over_budget = tabs.len().saturating_sub(self.max_live_tabs);
        eligible
            .into_iter()
            .enumerate()
            .filter(|(index, t)| *index < over_budget || t.idle >= self.idle_after)
            .map(|(_, t)| t.id)
            .collect()
    }
}
/// Whether macOS reports memory pressure (warning or critical).
pub fn system_pressure() -> bool {
    let mut level: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    let name = c"kern.memorystatus_vm_pressure_level";
    // SAFETY: `level` and `size` describe a writable c_int, as the sysctl expects.
    let ok = unsafe { libc::sysctlbyname(name.as_ptr(), (&mut level as *mut libc::c_int).cast(), &mut size, std::ptr::null_mut(), 0) } == 0;
    ok && level >= 2
}
#[cfg(test)]
mod tests {
    use super::*;
    fn tab(id: u64, idle: u64) -> Candidate {
        Candidate {
            id,
            idle: Duration::from_secs(idle),
            visible: false,
            loading: false,
            protected: false,
            frames: 1,
        }
    }
    #[test]
    fn evicts_oldest_background_renderers_to_budget() {
        let p = Policy {
            max_live_tabs: 2,
            ..Default::default()
        };
        assert_eq!(
            p.candidates(&[tab(1, 60), tab(2, 90), tab(3, 40), tab(4, 50)]),
            vec![2, 1]
        );
    }
    #[test]
    fn never_evicts_visible_loading_or_protected_tabs() {
        let mut a = tab(1, 600);
        a.visible = true;
        let mut b = tab(2, 600);
        b.protected = true;
        let mut c = tab(3, 600);
        c.loading = true;
        assert!(Policy {
            max_live_tabs: 1,
            ..Default::default()
        }
        .candidates(&[a, b, c])
        .is_empty());
    }
    #[test]
    fn evicts_heaviest_background_pages_first() {
        let mut heavy = tab(3, 40);
        heavy.frames = 60;
        assert_eq!(
            Policy {
                max_live_tabs: 2,
                ..Default::default()
            }
            .candidates(&[tab(1, 90), tab(2, 80), heavy]),
            vec![3]
        );
    }
    #[test]
    fn idle_tabs_are_released_even_when_under_budget() {
        assert_eq!(
            Policy::default().candidates(&[tab(1, 301), tab(2, 299)]),
            vec![1]
        );
    }
    #[test]
    fn memory_pressure_shrinks_the_budget_and_idle_timeout() {
        let p = Policy::default().under_pressure();
        assert_eq!(p.candidates(&[tab(1, 61), tab(2, 40), tab(3, 35), tab(4, 50)]), vec![1, 4]);
    }
    #[test]
    fn recently_used_tabs_are_not_thrashed_under_pressure() {
        assert!(Policy {
            max_live_tabs: 1,
            ..Default::default()
        }
        .candidates(&[tab(1, 5), tab(2, 20)])
        .is_empty());
    }
}
