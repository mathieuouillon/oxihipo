//! [`Context`] — the typed per-event store algorithms share data through.

use std::any::{Any, TypeId};
use std::collections::HashMap;

use hipo::EventCtx;

/// The per-event store: the raw event plus a type-keyed bag of derived
/// products that algorithms publish for later algorithms to consume.
///
/// A fresh `Context` is built for every event. One product is stored per
/// type — for several products of the same type, wrap them in newtypes
/// (`struct Electron(LorentzVector)`), the typed-product pattern.
pub struct Context<'a> {
    event: EventCtx<'a>,
    products: HashMap<TypeId, Box<dyn Any + Send>>,
}

impl<'a> Context<'a> {
    pub(crate) fn new(event: EventCtx<'a>) -> Self {
        Self {
            event,
            products: HashMap::new(),
        }
    }

    /// The raw event — e.g. `ctx.event().bank("REC::Particle")`.
    pub fn event(&self) -> &EventCtx<'a> {
        &self.event
    }

    /// Publish a derived product for later algorithms in the chain.
    /// A second `put` of the same type replaces the first.
    pub fn put<T: Any + Send>(&mut self, value: T) {
        self.products.insert(TypeId::of::<T>(), Box::new(value));
    }

    /// Retrieve a product published earlier in the chain, if any.
    pub fn get<T: Any + Send>(&self) -> Option<&T> {
        self.products
            .get(&TypeId::of::<T>())
            .and_then(|b| b.downcast_ref::<T>())
    }

    /// Whether a product of type `T` has been published.
    pub fn has<T: Any + Send>(&self) -> bool {
        self.products.contains_key(&TypeId::of::<T>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hipo::{Dict, Event};

    #[test]
    fn put_get_has() {
        let dict = Dict::new();
        let mut ctx = Context::new(EventCtx::new(Event::new(&[]), &dict));

        assert!(!ctx.has::<i32>());
        ctx.put(42_i32);
        ctx.put(String::from("hello"));

        assert_eq!(ctx.get::<i32>(), Some(&42));
        assert_eq!(ctx.get::<String>().map(String::as_str), Some("hello"));
        assert!(ctx.has::<i32>());
        assert_eq!(ctx.get::<f64>(), None);
    }
}
