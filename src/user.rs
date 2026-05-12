//! User and UserId types.

pub use crate::history::UserId;

pub struct User {
    id: UserId,
}

impl User {
    pub fn new() -> Self {
        Self { id: UserId::new() }
    }

    pub fn id(&self) -> UserId {
        self.id
    }
}

impl Default for User {
    fn default() -> Self {
        Self::new()
    }
}
