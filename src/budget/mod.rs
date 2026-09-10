//! Money, frozen at the start of the run and only ever spent down.

pub mod estimate;
pub mod price;

pub use estimate::{bytes_for_ascii_tokens, estimate_ascii_tokens, estimate_tokens};
pub use price::{Price, TokenUsage};

use crate::config::Selection;

#[derive(Debug, thiserror::Error)]
pub enum BudgetError {
    #[error(
        "budget exhausted: {spent:.4} {currency} spent, this call is estimated at {estimate:.4} {currency}, limit is {limit:.4} {currency}"
    )]
    Exhausted {
        spent: f64,
        estimate: f64,
        limit: f64,
        currency: String,
    },
    #[error("budget is 0 {currency}: this run may not spend anything")]
    SpendNothing { currency: String },
    #[error("budget {value} is not -1, 0 or a positive amount")]
    InvalidLimit { value: f64 },
}

/// The three shapes a `budget` value can take.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Limit {
    Unlimited,
    Nothing,
    Amount(f64),
}

impl Limit {
    /// `-1` is the only sentinel: `-10` is a typo, not a wish for no ceiling.
    pub fn from_value(value: f64) -> Result<Self, BudgetError> {
        if value == -1.0 {
            return Ok(Limit::Unlimited);
        }
        if value == 0.0 {
            return Ok(Limit::Nothing);
        }
        if value > 0.0 {
            return Ok(Limit::Amount(value));
        }
        Err(BudgetError::InvalidLimit { value })
    }

    /// The number written into `meta.json`, which keeps `-1` recognizable.
    pub fn as_value(self) -> f64 {
        match self {
            Limit::Unlimited => -1.0,
            Limit::Nothing => 0.0,
            Limit::Amount(amount) => amount,
        }
    }
}

/// The account for one run: one currency, one price list, no top ups.
#[derive(Clone, Debug)]
pub struct Budget {
    limit: Limit,
    currency: String,
    price: Price,
    spent: f64,
}

impl Budget {
    /// Freeze from the selected model and the provider it hangs off.
    pub fn freeze(selection: &Selection<'_>) -> Result<Self, BudgetError> {
        Ok(Self {
            limit: Limit::from_value(selection.provider.budget_per_run)?,
            currency: selection.provider.currency.clone(),
            price: Price::from_model(selection.model),
            spent: 0.0,
        })
    }

    /// Rebuild an in-progress account from what `meta.json` froze earlier.
    pub fn restore(limit: Limit, currency: String, price: Price, spent: f64) -> Self {
        Self {
            limit,
            currency,
            price,
            spent,
        }
    }

    pub fn limit(&self) -> Limit {
        self.limit
    }

    pub fn currency(&self) -> &str {
        &self.currency
    }

    pub fn price(&self) -> &Price {
        &self.price
    }

    pub fn spent(&self) -> f64 {
        self.spent
    }

    /// What one call is expected to cost, so the check below has a number.
    pub fn estimate(&self, input_tokens: u32, max_output_tokens: u32) -> f64 {
        self.price.cost(&TokenUsage {
            input_tokens,
            cached_input_tokens: 0,
            output_tokens: max_output_tokens,
        })
    }

    /// Run before every model call. Overspending is never repaired after
    /// the fact, so this is the only gate.
    pub fn check(&self, estimate: f64) -> Result<(), BudgetError> {
        match self.limit {
            Limit::Unlimited => Ok(()),
            Limit::Nothing => Err(BudgetError::SpendNothing {
                currency: self.currency.clone(),
            }),
            Limit::Amount(limit) if self.spent + estimate > limit => Err(BudgetError::Exhausted {
                spent: self.spent,
                estimate,
                limit,
                currency: self.currency.clone(),
            }),
            Limit::Amount(_) => Ok(()),
        }
    }

    /// Replace the estimate with what the response actually cost.
    pub fn settle(&mut self, usage: &TokenUsage) -> f64 {
        let cost = self.price.cost(usage);
        self.spent += cost;
        cost
    }

    /// `Some(limit)` for a real ceiling, `None` when unlimited: the CLI
    /// prints "spent X (no ceiling)" rather than leaving the line blank.
    pub fn ceiling(&self) -> Option<f64> {
        match self.limit {
            Limit::Amount(amount) => Some(amount),
            Limit::Nothing => Some(0.0),
            Limit::Unlimited => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price() -> Price {
        Price {
            input_per_1m_tokens: 2.0,
            cached_input_per_1m_tokens: Some(0.2),
            output_per_1m_tokens: 3.0,
        }
    }

    fn budget(limit: Limit) -> Budget {
        Budget::restore(limit, "CNY".to_string(), price(), 0.0)
    }

    #[test]
    fn only_minus_one_means_unlimited() {
        assert_eq!(Limit::from_value(-1.0).unwrap(), Limit::Unlimited);
        assert_eq!(Limit::from_value(0.0).unwrap(), Limit::Nothing);
        assert_eq!(Limit::from_value(10.0).unwrap(), Limit::Amount(10.0));
        assert!(Limit::from_value(-2.0).is_err());
    }

    #[test]
    fn zero_stops_before_the_first_call_and_unlimited_never_stops() {
        assert!(matches!(
            budget(Limit::Nothing).check(0.0),
            Err(BudgetError::SpendNothing { .. })
        ));
        assert!(budget(Limit::Unlimited).check(1_000_000.0).is_ok());
    }

    #[test]
    fn a_ceiling_stops_the_call_that_would_cross_it() {
        let mut account = budget(Limit::Amount(1.0));
        account.settle(&TokenUsage {
            input_tokens: 100_000,
            cached_input_tokens: 0,
            output_tokens: 100_000,
        });
        assert!((account.spent() - 0.5).abs() < 1e-9, "{}", account.spent());
        assert!(account.check(0.4).is_ok());
        assert!(matches!(
            account.check(0.6),
            Err(BudgetError::Exhausted { .. })
        ));
    }
}
