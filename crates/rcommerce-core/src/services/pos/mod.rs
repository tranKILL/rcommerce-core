//! Point of sale: till sessions and their cash, card terminals, codes scanned
//! at the till, sales and returns at the counter. Everything here needs the
//! `pos` permission (see `routes/pos.rs`); prices, promotions, stock, points
//! and invoices go through the same services as the web shop.

pub mod basket;
pub mod gateway;
pub mod lookup;
pub mod session;
pub mod terminal;

pub use lookup::{lookup, Lookup};
pub use session::{TillSession, TillSessions, ZReport};
pub use terminal::{PaymentTerminal, TerminalTransaction, Terminals};
