use gpui::{IntoElement, ParentElement};
use ui::{List, ListBulletItem, prelude::*};

/// Centralized definitions for Zed AI plans
pub struct PlanDefinitions;

impl PlanDefinitions {
    pub fn free_plan(&self) -> impl IntoElement {
        List::new()
            .child(ListBulletItem::new("採用された編集予測 2,000 件"))
            .child(ListBulletItem::new(
                "自分の AI API キーによる無制限のプロンプト",
            ))
            .child(ListBulletItem::new("外部エージェントの無制限利用"))
    }

    pub fn sign_in_upsell(&self) -> impl IntoElement {
        List::new()
            .child(ListBulletItem::new("無制限の編集予測"))
            .child(ListBulletItem::new(
                "Zed エージェントで使えるトークン $20 分",
            ))
            .child(ListBulletItem::new("クレジットカード不要"))
    }

    pub fn pro_trial(&self, period: bool) -> impl IntoElement {
        List::new()
            .child(ListBulletItem::new(
                "Zed エージェントで使えるトークン $20 分",
            ))
            .child(ListBulletItem::new("無制限の編集予測"))
            .when(period, |this| {
                this.child(ListBulletItem::new(
                    "14 日間お試し可能、クレジットカード不要",
                ))
            })
    }

    pub fn pro_plan(&self) -> impl IntoElement {
        List::new()
            .child(ListBulletItem::new(
                "Zed エージェントで使えるトークン $5 分",
            ))
            .child(ListBulletItem::new("$5 を超える分は従量課金"))
            .child(ListBulletItem::new("無制限の編集予測"))
    }

    pub fn business_plan(&self) -> impl IntoElement {
        List::new()
            .child(ListBulletItem::new("無制限の編集予測"))
            .child(ListBulletItem::new("従量課金"))
    }

    pub fn vip_plan(&self) -> impl IntoElement {
        List::new()
            .child(ListBulletItem::new("無制限の編集予測"))
            .child(ListBulletItem::new("Zed エージェントで使えるトークン"))
    }

    pub fn student_plan(&self) -> impl IntoElement {
        List::new()
            .child(ListBulletItem::new("無制限の編集予測"))
            .child(ListBulletItem::new(
                "Zed エージェントで使えるトークン $10 分",
            ))
            .child(ListBulletItem::new(
                "追加利用向けのオプションのクレジットパック",
            ))
    }
}
