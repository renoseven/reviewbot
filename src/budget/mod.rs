//! Money, frozen at the start of the run and only ever spent down.

pub mod estimate;
pub mod price;

pub use estimate::{bytes_for_ascii_tokens, estimate_ascii_tokens, estimate_tokens};
pub use price::{Price, TokenUsage};

use crate::config::Selection;

/// The smallest output allowance worth paying for. A reply that cannot fit
/// a tool call and its arguments arrives truncated, so the last of the money
/// goes on nothing; below this the budget refuses instead.
const LEAST_USEFUL_OUTPUT_TOKENS: u32 = 1_024;

#[derive(Debug, thiserror::Error)]
pub enum BudgetError {
    #[error(
        "budget exhausted: {spent:.4} of {limit:.4} {currency} spent, too little left to answer with"
    )]
    Exhausted {
        spent: f64,
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
///
/// It only ever counts money the vendor has actually charged. Nothing here
/// predicts what a call will cost — two attempts at that were wrong in turn
/// (once by charging every call for a full thinking ceiling, once by pricing
/// input as though the prompt cache did not exist) and both refused calls
/// the budget could pay for many times over. What is left is arithmetic with
/// no guess in it: output has a known price and is never cached, so the money
/// on hand converts exactly into output tokens, and that is the number the
/// vendor is held to.
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

    /// The output allowance for one call: never more than `ceiling`, never
    /// more than the money on hand buys. The answer is written onto the
    /// request, so the vendor is held to it rather than trusted to stay
    /// under a number it was never told.
    ///
    /// `reserve` is the output allowance a **later** call must still be able
    /// to ask for. A caller with somewhere to hand its work over passes it,
    /// and so stops while it can still afford to stop well; a caller with
    /// nothing to follow passes zero.
    ///
    /// Input is not charged for in advance, and that is deliberate: what it
    /// will cost depends on how much of the prompt the vendor serves from
    /// its cache, which is unknowable before the reply and was measured at
    /// 97% on a real run. Guessing it is what broke this twice. The price is
    /// that one call may carry the ceiling past its limit by its own input —
    /// the small, mostly cached half — which `settle` then records.
    ///
    /// `Err` means this call must not go out: after the reserve there is not
    /// enough left to answer with.
    pub fn allow(&self, ceiling: u32, reserve: u32) -> Result<u32, BudgetError> {
        let limit = match self.limit {
            Limit::Unlimited => return Ok(ceiling),
            Limit::Nothing => {
                return Err(BudgetError::SpendNothing {
                    currency: self.currency.clone(),
                });
            }
            Limit::Amount(limit) => limit,
        };
        // Both sides in output tokens, so holding room back is a
        // subtraction rather than a second price calculation.
        let affordable = self
            .price
            .output_tokens_for(limit - self.spent)
            .saturating_sub(reserve);
        let allowed = ceiling.min(affordable);
        // A ceiling below the floor is a caller that wants a short answer on
        // purpose, and it gets one. Anything else this small buys a reply
        // cut off mid-sentence, which is money spent on nothing.
        match allowed >= LEAST_USEFUL_OUTPUT_TOKENS.min(ceiling) {
            true => Ok(allowed),
            false => Err(BudgetError::Exhausted {
                spent: self.spent,
                limit,
                currency: self.currency.clone(),
            }),
        }
    }

    /// Add what the response really cost. The only place `spent` moves:
    /// every number this type reports is one the vendor charged.
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
            budget(Limit::Nothing).allow(100, 0),
            Err(BudgetError::SpendNothing { .. })
        ));
        assert_eq!(
            budget(Limit::Unlimited).allow(100, 4_096).unwrap(),
            100,
            "no ceiling means nothing to hold back from"
        );
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
        // 0.5 left, output at 3.0 per 1M: 100k more output fits, 200k does not.
        assert_eq!(account.allow(100_000, 0).unwrap(), 100_000);
        assert_eq!(account.allow(200_000, 0).unwrap(), 166_666);
    }

    fn flash() -> Price {
        Price {
            input_per_1m_tokens: 3.0,
            cached_input_per_1m_tokens: Some(0.1),
            output_per_1m_tokens: 9.0,
        }
    }

    fn flash_budget(limit: f64) -> Budget {
        Budget::restore(Limit::Amount(limit), "CNY".to_string(), flash(), 0.0)
    }

    /// 384K of thinking at 9 CNY/1M is 3.456 CNY. Charging a 0.10 CNY run
    /// for all of it up front refused the call outright; capping the
    /// vendor to what 0.10 buys sends it.
    #[test]
    fn what_is_left_converts_straight_into_an_output_allowance() {
        // 0.10 CNY at 9.0 per 1M output is 11111 tokens, and that is the
        // whole calculation: no input, no cache, nothing to guess.
        assert_eq!(flash_budget(0.1).allow(384_000, 0).unwrap(), 11_111);
        assert_eq!(
            flash_budget(10.0).allow(384_000, 4_096).unwrap(),
            384_000,
            "a budget that covers the model's ceiling leaves it alone"
        );
    }

    /// The floor exists so the last of the money is not spent on a reply
    /// that arrives cut off mid-sentence.
    #[test]
    fn an_allowance_too_small_to_answer_with_is_refused_rather_than_sent() {
        let mut account = flash_budget(0.1);
        account.settle(&TokenUsage {
            input_tokens: 0,
            cached_input_tokens: 0,
            output_tokens: 10_945,
        });
        // Roughly 0.0015 left: 160-odd output tokens, well under the floor.
        assert!(matches!(
            account.allow(384_000, 0),
            Err(BudgetError::Exhausted { .. })
        ));
        assert_eq!(
            account.allow(64, 0).unwrap(),
            64,
            "a caller that asked for a short answer still gets one"
        );
    }

    /// Stopping well costs a call, so the room for it is kept back before
    /// the money is gone rather than discovered afterwards.
    #[test]
    fn a_reserve_stops_the_investigation_while_the_conclusion_is_still_affordable() {
        let mut account = flash_budget(0.1);
        account.settle(&TokenUsage {
            input_tokens: 0,
            cached_input_tokens: 0,
            output_tokens: 10_000,
        });
        // 0.01 CNY left, which is 1111 output tokens: enough to conclude
        // with, not enough to both investigate and still conclude.
        assert!(matches!(
            account.allow(384_000, 4_096),
            Err(BudgetError::Exhausted { .. })
        ));
        assert_eq!(
            account.allow(4_096, 0).unwrap(),
            1_111,
            "and the conclusion gets what is actually left, not the 4096 it asked for"
        );
    }

    /// Input is charged when the vendor bills it, not before. The reply
    /// that used to be estimated at 0.0525 and refused cost 0.0038.
    #[test]
    fn input_is_counted_when_it_is_billed_rather_than_predicted() {
        let mut account = flash_budget(0.1);
        let before = account.allow(384_000, 0).unwrap();

        account.settle(&TokenUsage {
            input_tokens: 17_500,
            cached_input_tokens: 17_024,
            output_tokens: 79,
        });
        assert!(
            (account.spent() - 0.0038).abs() < 0.0002,
            "the vendor's own number, cache and all: {}",
            account.spent()
        );
        assert!(
            account.allow(384_000, 0).unwrap() < before,
            "and the next allowance shrinks by exactly what was charged"
        );
    }
}
