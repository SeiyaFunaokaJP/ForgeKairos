use gpui::{IntoElement, ParentElement};
use ui::{List, ListBulletItem, prelude::*};

/// Centralized definitions for Zed AI plans
pub struct PlanDefinitions;

impl PlanDefinitions {
    pub fn free_plan(&self) -> impl IntoElement {
        List::new()
            .child(ListBulletItem::new(i18n::t!(
                "Unlimited prompts with your AI API keys"
            )))
            .child(ListBulletItem::new(i18n::t!(
                "Unlimited use of external agents"
            )))
    }

    pub fn sign_in_upsell(&self) -> impl IntoElement {
        List::new()
            .child(ListBulletItem::new(i18n::t!("Unlimited edit predictions")))
            .child(ListBulletItem::new(i18n::t!("$5 of GPT Luna")))
            .child(ListBulletItem::new(i18n::t!("No credit card required")))
    }

    pub fn pro_trial(&self, period: bool) -> impl IntoElement {
        List::new()
            .child(ListBulletItem::new(i18n::t!("$5 of GPT Luna")))
            .child(ListBulletItem::new(i18n::t!("Unlimited edit predictions")))
            .when(period, |this| {
                this.child(ListBulletItem::new(i18n::t!(
                    "14 days from trial start, no credit card required"
                )))
            })
    }

    pub fn pro_plan(&self) -> impl IntoElement {
        List::new()
            .child(ListBulletItem::new(i18n::t!("$5 of tokens in Zed agent")))
            .child(ListBulletItem::new(i18n::t!(
                "Usage-based billing beyond $5"
            )))
            .child(ListBulletItem::new(i18n::t!("Unlimited edit predictions")))
    }

    pub fn business_plan(&self) -> impl IntoElement {
        List::new()
            .child(ListBulletItem::new(i18n::t!("Unlimited edit predictions")))
            .child(ListBulletItem::new(i18n::t!("Usage-based billing")))
    }

    pub fn vip_plan(&self) -> impl IntoElement {
        List::new()
            .child(ListBulletItem::new(i18n::t!("Unlimited edit predictions")))
            .child(ListBulletItem::new(i18n::t!("Tokens in the Zed agent")))
    }

    pub fn student_plan(&self) -> impl IntoElement {
        List::new()
            .child(ListBulletItem::new(i18n::t!("Unlimited edit predictions")))
            .child(ListBulletItem::new(i18n::t!("$10 of tokens in Zed agent")))
            .child(ListBulletItem::new(i18n::t!(
                "Optional credit packs for additional usage"
            )))
    }
}
