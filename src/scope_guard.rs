pub struct ScopeGuard<F: FnMut()> {
    f: F,
}

/// Execute `f` at end of scope unless `release` has been called
pub fn scope_guard<F: FnMut()>(f: F) -> ScopeGuard<F> {
    ScopeGuard::<F> { f }
}

impl<F: FnMut()> ScopeGuard<F> {
    /// Indicate that the danger is over and that we could nut run `f` at the end of scope
    pub fn release(self) {
        std::mem::forget(self);
    }
}

impl<F: FnMut()> Drop for ScopeGuard<F> {
    fn drop(&mut self) {
        (self.f)()
    }
}
