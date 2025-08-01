use actix_web::http::header::HeaderMap;
use api_models::{
    cards_info as card_info_types, enums as api_enums, gsm as gsm_api_types, payment_methods,
    payments, routing::ConnectorSelection,
};
use common_utils::{
    consts::X_HS_LATENCY,
    crypto::Encryptable,
    ext_traits::{Encode, StringExt, ValueExt},
    fp_utils::when,
    pii,
    types::ConnectorTransactionIdTrait,
};
use diesel_models::enums as storage_enums;
use error_stack::{report, ResultExt};
use hyperswitch_domain_models::payments::payment_intent::CustomerData;
use masking::{ExposeInterface, PeekInterface, Secret};

use super::domain;
use crate::{
    core::errors,
    headers::{
        ACCEPT_LANGUAGE, BROWSER_NAME, X_APP_ID, X_CLIENT_PLATFORM, X_CLIENT_SOURCE,
        X_CLIENT_VERSION, X_MERCHANT_DOMAIN, X_PAYMENT_CONFIRM_SOURCE, X_REDIRECT_URI,
    },
    services::authentication::get_header_value_by_key,
    types::{
        self as router_types,
        api::{self as api_types, routing as routing_types},
        storage,
    },
};

pub trait ForeignInto<T> {
    fn foreign_into(self) -> T;
}

pub trait ForeignTryInto<T> {
    type Error;

    fn foreign_try_into(self) -> Result<T, Self::Error>;
}

pub trait ForeignFrom<F> {
    fn foreign_from(from: F) -> Self;
}

pub trait ForeignTryFrom<F>: Sized {
    type Error;

    fn foreign_try_from(from: F) -> Result<Self, Self::Error>;
}

impl<F, T> ForeignInto<T> for F
where
    T: ForeignFrom<F>,
{
    fn foreign_into(self) -> T {
        T::foreign_from(self)
    }
}

impl<F, T> ForeignTryInto<T> for F
where
    T: ForeignTryFrom<F>,
{
    type Error = <T as ForeignTryFrom<F>>::Error;

    fn foreign_try_into(self) -> Result<T, Self::Error> {
        T::foreign_try_from(self)
    }
}

impl ForeignFrom<api_models::refunds::RefundType> for storage_enums::RefundType {
    fn foreign_from(item: api_models::refunds::RefundType) -> Self {
        match item {
            api_models::refunds::RefundType::Instant => Self::InstantRefund,
            api_models::refunds::RefundType::Scheduled => Self::RegularRefund,
        }
    }
}

#[cfg(all(
    any(feature = "v1", feature = "v2"),
    not(feature = "payment_methods_v2")
))]
impl
    ForeignFrom<(
        Option<payment_methods::CardDetailFromLocker>,
        domain::PaymentMethod,
    )> for payment_methods::PaymentMethodResponse
{
    fn foreign_from(
        (card_details, item): (
            Option<payment_methods::CardDetailFromLocker>,
            domain::PaymentMethod,
        ),
    ) -> Self {
        Self {
            merchant_id: item.merchant_id.to_owned(),
            customer_id: Some(item.customer_id.to_owned()),
            payment_method_id: item.get_id().clone(),
            payment_method: item.get_payment_method_type(),
            payment_method_type: item.get_payment_method_subtype(),
            card: card_details,
            recurring_enabled: false,
            installment_payment_enabled: false,
            payment_experience: None,
            metadata: item.metadata,
            created: Some(item.created_at),
            #[cfg(feature = "payouts")]
            bank_transfer: None,
            last_used_at: None,
            client_secret: item.client_secret,
        }
    }
}

#[cfg(all(feature = "v2", feature = "payment_methods_v2"))]
impl
    ForeignFrom<(
        Option<payment_methods::CardDetailFromLocker>,
        domain::PaymentMethod,
    )> for payment_methods::PaymentMethodResponse
{
    fn foreign_from(
        (card_details, item): (
            Option<payment_methods::CardDetailFromLocker>,
            domain::PaymentMethod,
        ),
    ) -> Self {
        todo!()
    }
}

// TODO: remove this usage in v1 code
impl ForeignFrom<storage_enums::AttemptStatus> for storage_enums::IntentStatus {
    fn foreign_from(s: storage_enums::AttemptStatus) -> Self {
        Self::from(s)
    }
}

impl ForeignTryFrom<storage_enums::AttemptStatus> for storage_enums::CaptureStatus {
    type Error = error_stack::Report<errors::ApiErrorResponse>;

    fn foreign_try_from(
        attempt_status: storage_enums::AttemptStatus,
    ) -> errors::RouterResult<Self> {
        match attempt_status {
            storage_enums::AttemptStatus::Charged
            | storage_enums::AttemptStatus::PartialCharged => Ok(Self::Charged),
            storage_enums::AttemptStatus::Pending
            | storage_enums::AttemptStatus::CaptureInitiated => Ok(Self::Pending),
            storage_enums::AttemptStatus::Failure
            | storage_enums::AttemptStatus::CaptureFailed => Ok(Self::Failed),

            storage_enums::AttemptStatus::Started
            | storage_enums::AttemptStatus::AuthenticationFailed
            | storage_enums::AttemptStatus::RouterDeclined
            | storage_enums::AttemptStatus::AuthenticationPending
            | storage_enums::AttemptStatus::AuthenticationSuccessful
            | storage_enums::AttemptStatus::Authorized
            | storage_enums::AttemptStatus::AuthorizationFailed
            | storage_enums::AttemptStatus::Authorizing
            | storage_enums::AttemptStatus::CodInitiated
            | storage_enums::AttemptStatus::Voided
            | storage_enums::AttemptStatus::VoidInitiated
            | storage_enums::AttemptStatus::VoidFailed
            | storage_enums::AttemptStatus::AutoRefunded
            | storage_enums::AttemptStatus::Unresolved
            | storage_enums::AttemptStatus::PaymentMethodAwaited
            | storage_enums::AttemptStatus::ConfirmationAwaited
            | storage_enums::AttemptStatus::DeviceDataCollectionPending
            | storage_enums::AttemptStatus::PartialChargedAndChargeable=> {
                Err(errors::ApiErrorResponse::PreconditionFailed {
                    message: "AttemptStatus must be one of these for multiple partial captures [Charged, PartialCharged, Pending, CaptureInitiated, Failure, CaptureFailed]".into(),
                }.into())
            }
        }
    }
}

impl ForeignFrom<payments::MandateType> for storage_enums::MandateDataType {
    fn foreign_from(from: payments::MandateType) -> Self {
        match from {
            payments::MandateType::SingleUse(inner) => Self::SingleUse(inner.foreign_into()),
            payments::MandateType::MultiUse(inner) => {
                Self::MultiUse(inner.map(ForeignInto::foreign_into))
            }
        }
    }
}

impl ForeignFrom<storage_enums::MandateDataType> for payments::MandateType {
    fn foreign_from(from: storage_enums::MandateDataType) -> Self {
        match from {
            storage_enums::MandateDataType::SingleUse(inner) => {
                Self::SingleUse(inner.foreign_into())
            }
            storage_enums::MandateDataType::MultiUse(inner) => {
                Self::MultiUse(inner.map(ForeignInto::foreign_into))
            }
        }
    }
}

impl ForeignTryFrom<api_enums::Connector> for common_enums::RoutableConnectors {
    type Error = error_stack::Report<common_utils::errors::ValidationError>;

    fn foreign_try_from(from: api_enums::Connector) -> Result<Self, Self::Error> {
        Ok(match from {
            api_enums::Connector::Aci => Self::Aci,
            api_enums::Connector::Adyen => Self::Adyen,
            api_enums::Connector::Adyenplatform => Self::Adyenplatform,
            api_enums::Connector::Airwallex => Self::Airwallex,
            // api_enums::Connector::Amazonpay => Self::Amazonpay,
            api_enums::Connector::Archipel => Self::Archipel,
            api_enums::Connector::Authorizedotnet => Self::Authorizedotnet,
            api_enums::Connector::Bambora => Self::Bambora,
            api_enums::Connector::Bamboraapac => Self::Bamboraapac,
            api_enums::Connector::Bankofamerica => Self::Bankofamerica,
            api_enums::Connector::Barclaycard => Self::Barclaycard,
            api_enums::Connector::Billwerk => Self::Billwerk,
            api_enums::Connector::Bitpay => Self::Bitpay,
            api_enums::Connector::Bluesnap => Self::Bluesnap,
            api_enums::Connector::Boku => Self::Boku,
            api_enums::Connector::Braintree => Self::Braintree,
            api_enums::Connector::Cashtocode => Self::Cashtocode,
            api_enums::Connector::Chargebee => Self::Chargebee,
            api_enums::Connector::Checkout => Self::Checkout,
            api_enums::Connector::Coinbase => Self::Coinbase,
            api_enums::Connector::Coingate => Self::Coingate,
            api_enums::Connector::Cryptopay => Self::Cryptopay,
            api_enums::Connector::CtpVisa => {
                Err(common_utils::errors::ValidationError::InvalidValue {
                    message: "ctp visa is not a routable connector".to_string(),
                })?
            }
            api_enums::Connector::CtpMastercard => {
                Err(common_utils::errors::ValidationError::InvalidValue {
                    message: "ctp mastercard is not a routable connector".to_string(),
                })?
            }
            api_enums::Connector::Cybersource => Self::Cybersource,
            api_enums::Connector::Datatrans => Self::Datatrans,
            api_enums::Connector::Deutschebank => Self::Deutschebank,
            api_enums::Connector::Digitalvirgo => Self::Digitalvirgo,
            api_enums::Connector::Dlocal => Self::Dlocal,
            api_enums::Connector::Ebanx => Self::Ebanx,
            api_enums::Connector::Elavon => Self::Elavon,
            api_enums::Connector::Facilitapay => Self::Facilitapay,
            api_enums::Connector::Fiserv => Self::Fiserv,
            api_enums::Connector::Fiservemea => Self::Fiservemea,
            api_enums::Connector::Fiuu => Self::Fiuu,
            api_enums::Connector::Forte => Self::Forte,
            api_enums::Connector::Getnet => Self::Getnet,
            api_enums::Connector::Globalpay => Self::Globalpay,
            api_enums::Connector::Globepay => Self::Globepay,
            api_enums::Connector::Gocardless => Self::Gocardless,
            api_enums::Connector::Gpayments => {
                Err(common_utils::errors::ValidationError::InvalidValue {
                    message: "gpayments is not a routable connector".to_string(),
                })?
            }
            api_enums::Connector::Hipay => Self::Hipay,
            api_enums::Connector::Helcim => Self::Helcim,
            api_enums::Connector::HyperswitchVault => {
                Err(common_utils::errors::ValidationError::InvalidValue {
                    message: "Hyperswitch Vault is not a routable connector".to_string(),
                })?
            }
            api_enums::Connector::Iatapay => Self::Iatapay,
            api_enums::Connector::Inespay => Self::Inespay,
            api_enums::Connector::Itaubank => Self::Itaubank,
            api_enums::Connector::Jpmorgan => Self::Jpmorgan,
            api_enums::Connector::Juspaythreedsserver => {
                Err(common_utils::errors::ValidationError::InvalidValue {
                    message: "juspaythreedsserver is not a routable connector".to_string(),
                })?
            }
            api_enums::Connector::Klarna => Self::Klarna,
            api_enums::Connector::Mifinity => Self::Mifinity,
            api_enums::Connector::Mollie => Self::Mollie,
            api_enums::Connector::Moneris => Self::Moneris,
            api_enums::Connector::Multisafepay => Self::Multisafepay,
            api_enums::Connector::Netcetera => {
                Err(common_utils::errors::ValidationError::InvalidValue {
                    message: "netcetera is not a routable connector".to_string(),
                })?
            }
            api_enums::Connector::Nexinets => Self::Nexinets,
            api_enums::Connector::Nexixpay => Self::Nexixpay,
            api_enums::Connector::Nmi => Self::Nmi,
            api_enums::Connector::Nomupay => Self::Nomupay,
            api_enums::Connector::Noon => Self::Noon,
            // api_enums::Connector::Nordea => Self::Nordea,
            api_enums::Connector::Novalnet => Self::Novalnet,
            api_enums::Connector::Nuvei => Self::Nuvei,
            api_enums::Connector::Opennode => Self::Opennode,
            api_enums::Connector::Paybox => Self::Paybox,
            api_enums::Connector::Payme => Self::Payme,
            api_enums::Connector::Payone => Self::Payone,
            api_enums::Connector::Paypal => Self::Paypal,
            api_enums::Connector::Paystack => Self::Paystack,
            api_enums::Connector::Payu => Self::Payu,
            api_models::enums::Connector::Placetopay => Self::Placetopay,
            api_enums::Connector::Plaid => Self::Plaid,
            api_enums::Connector::Powertranz => Self::Powertranz,
            api_enums::Connector::Prophetpay => Self::Prophetpay,
            api_enums::Connector::Rapyd => Self::Rapyd,
            api_enums::Connector::Razorpay => Self::Razorpay,
            api_enums::Connector::Recurly => Self::Recurly,
            api_enums::Connector::Redsys => Self::Redsys,
            api_enums::Connector::Shift4 => Self::Shift4,
            api_enums::Connector::Signifyd => {
                Err(common_utils::errors::ValidationError::InvalidValue {
                    message: "signifyd is not a routable connector".to_string(),
                })?
            }
            api_enums::Connector::Riskified => {
                Err(common_utils::errors::ValidationError::InvalidValue {
                    message: "riskified is not a routable connector".to_string(),
                })?
            }
            api_enums::Connector::Square => Self::Square,
            api_enums::Connector::Stax => Self::Stax,
            api_enums::Connector::Stripe => Self::Stripe,
            api_enums::Connector::Stripebilling => Self::Stripebilling,
            // api_enums::Connector::Taxjar => Self::Taxjar,
            // api_enums::Connector::Thunes => Self::Thunes,
            // api_enums::Connector::Tokenio => Self::Tokenio,
            api_enums::Connector::Trustpay => Self::Trustpay,
            api_enums::Connector::Tsys => Self::Tsys,
            // api_enums::Connector::UnifiedAuthenticationService => {
            //     Self::UnifiedAuthenticationService
            // }
            api_enums::Connector::Vgs => {
                Err(common_utils::errors::ValidationError::InvalidValue {
                    message: "Vgs is not a routable connector".to_string(),
                })?
            }
            api_enums::Connector::Volt => Self::Volt,
            api_enums::Connector::Wellsfargo => Self::Wellsfargo,
            // api_enums::Connector::Wellsfargopayout => Self::Wellsfargopayout,
            api_enums::Connector::Wise => Self::Wise,
            api_enums::Connector::Worldline => Self::Worldline,
            api_enums::Connector::Worldpay => Self::Worldpay,
            api_enums::Connector::Worldpayvantiv => Self::Worldpayvantiv,
            api_enums::Connector::Worldpayxml => Self::Worldpayxml,
            api_enums::Connector::Xendit => Self::Xendit,
            api_enums::Connector::Zen => Self::Zen,
            api_enums::Connector::Zsl => Self::Zsl,
            #[cfg(feature = "dummy_connector")]
            api_enums::Connector::DummyBillingConnector => {
                Err(common_utils::errors::ValidationError::InvalidValue {
                    message: "stripe_billing_test is not a routable connector".to_string(),
                })?
            }
            #[cfg(feature = "dummy_connector")]
            api_enums::Connector::DummyConnector1 => Self::DummyConnector1,
            #[cfg(feature = "dummy_connector")]
            api_enums::Connector::DummyConnector2 => Self::DummyConnector2,
            #[cfg(feature = "dummy_connector")]
            api_enums::Connector::DummyConnector3 => Self::DummyConnector3,
            #[cfg(feature = "dummy_connector")]
            api_enums::Connector::DummyConnector4 => Self::DummyConnector4,
            #[cfg(feature = "dummy_connector")]
            api_enums::Connector::DummyConnector5 => Self::DummyConnector5,
            #[cfg(feature = "dummy_connector")]
            api_enums::Connector::DummyConnector6 => Self::DummyConnector6,
            #[cfg(feature = "dummy_connector")]
            api_enums::Connector::DummyConnector7 => Self::DummyConnector7,
            api_enums::Connector::Threedsecureio => {
                Err(common_utils::errors::ValidationError::InvalidValue {
                    message: "threedsecureio is not a routable connector".to_string(),
                })?
            }
            api_enums::Connector::Taxjar => {
                Err(common_utils::errors::ValidationError::InvalidValue {
                    message: "Taxjar is not a routable connector".to_string(),
                })?
            }
        })
    }
}

impl ForeignFrom<storage_enums::MandateAmountData> for payments::MandateAmountData {
    fn foreign_from(from: storage_enums::MandateAmountData) -> Self {
        Self {
            amount: from.amount,
            currency: from.currency,
            start_date: from.start_date,
            end_date: from.end_date,
            metadata: from.metadata,
        }
    }
}

// TODO: remove foreign from since this conversion won't be needed in the router crate once data models is treated as a single & primary source of truth for structure information
impl ForeignFrom<payments::MandateData> for hyperswitch_domain_models::mandates::MandateData {
    fn foreign_from(d: payments::MandateData) -> Self {
        Self {
            customer_acceptance: d.customer_acceptance.map(|d| {
                hyperswitch_domain_models::mandates::CustomerAcceptance {
                    acceptance_type: match d.acceptance_type {
                        payments::AcceptanceType::Online => {
                            hyperswitch_domain_models::mandates::AcceptanceType::Online
                        }
                        payments::AcceptanceType::Offline => {
                            hyperswitch_domain_models::mandates::AcceptanceType::Offline
                        }
                    },
                    accepted_at: d.accepted_at,
                    online: d
                        .online
                        .map(|d| hyperswitch_domain_models::mandates::OnlineMandate {
                            ip_address: d.ip_address,
                            user_agent: d.user_agent,
                        }),
                }
            }),
            mandate_type: d.mandate_type.map(|d| match d {
                payments::MandateType::MultiUse(Some(i)) => {
                    hyperswitch_domain_models::mandates::MandateDataType::MultiUse(Some(
                        hyperswitch_domain_models::mandates::MandateAmountData {
                            amount: i.amount,
                            currency: i.currency,
                            start_date: i.start_date,
                            end_date: i.end_date,
                            metadata: i.metadata,
                        },
                    ))
                }
                payments::MandateType::SingleUse(i) => {
                    hyperswitch_domain_models::mandates::MandateDataType::SingleUse(
                        hyperswitch_domain_models::mandates::MandateAmountData {
                            amount: i.amount,
                            currency: i.currency,
                            start_date: i.start_date,
                            end_date: i.end_date,
                            metadata: i.metadata,
                        },
                    )
                }
                payments::MandateType::MultiUse(None) => {
                    hyperswitch_domain_models::mandates::MandateDataType::MultiUse(None)
                }
            }),
            update_mandate_id: d.update_mandate_id,
        }
    }
}

impl ForeignFrom<payments::MandateAmountData> for storage_enums::MandateAmountData {
    fn foreign_from(from: payments::MandateAmountData) -> Self {
        Self {
            amount: from.amount,
            currency: from.currency,
            start_date: from.start_date,
            end_date: from.end_date,
            metadata: from.metadata,
        }
    }
}

impl ForeignFrom<api_enums::IntentStatus> for Option<storage_enums::EventType> {
    fn foreign_from(value: api_enums::IntentStatus) -> Self {
        match value {
            api_enums::IntentStatus::Succeeded => Some(storage_enums::EventType::PaymentSucceeded),
            api_enums::IntentStatus::Failed => Some(storage_enums::EventType::PaymentFailed),
            api_enums::IntentStatus::Processing => {
                Some(storage_enums::EventType::PaymentProcessing)
            }
            api_enums::IntentStatus::RequiresMerchantAction
            | api_enums::IntentStatus::RequiresCustomerAction => {
                Some(storage_enums::EventType::ActionRequired)
            }
            api_enums::IntentStatus::Cancelled => Some(storage_enums::EventType::PaymentCancelled),
            api_enums::IntentStatus::PartiallyCaptured
            | api_enums::IntentStatus::PartiallyCapturedAndCapturable => {
                Some(storage_enums::EventType::PaymentCaptured)
            }
            api_enums::IntentStatus::RequiresCapture => {
                Some(storage_enums::EventType::PaymentAuthorized)
            }
            api_enums::IntentStatus::RequiresPaymentMethod
            | api_enums::IntentStatus::RequiresConfirmation => None,
        }
    }
}

impl ForeignFrom<api_enums::PaymentMethodType> for api_enums::PaymentMethod {
    fn foreign_from(payment_method_type: api_enums::PaymentMethodType) -> Self {
        match payment_method_type {
            api_enums::PaymentMethodType::AmazonPay
            | api_enums::PaymentMethodType::ApplePay
            | api_enums::PaymentMethodType::GooglePay
            | api_enums::PaymentMethodType::Paypal
            | api_enums::PaymentMethodType::AliPay
            | api_enums::PaymentMethodType::AliPayHk
            | api_enums::PaymentMethodType::Dana
            | api_enums::PaymentMethodType::MbWay
            | api_enums::PaymentMethodType::MobilePay
            | api_enums::PaymentMethodType::Paze
            | api_enums::PaymentMethodType::SamsungPay
            | api_enums::PaymentMethodType::Twint
            | api_enums::PaymentMethodType::Vipps
            | api_enums::PaymentMethodType::TouchNGo
            | api_enums::PaymentMethodType::Swish
            | api_enums::PaymentMethodType::WeChatPay
            | api_enums::PaymentMethodType::GoPay
            | api_enums::PaymentMethodType::Gcash
            | api_enums::PaymentMethodType::Momo
            | api_enums::PaymentMethodType::Cashapp
            | api_enums::PaymentMethodType::KakaoPay
            | api_enums::PaymentMethodType::Venmo
            | api_enums::PaymentMethodType::Mifinity
            | api_enums::PaymentMethodType::RevolutPay => Self::Wallet,
            api_enums::PaymentMethodType::Affirm
            | api_enums::PaymentMethodType::Alma
            | api_enums::PaymentMethodType::AfterpayClearpay
            | api_enums::PaymentMethodType::Klarna
            | api_enums::PaymentMethodType::PayBright
            | api_enums::PaymentMethodType::Atome
            | api_enums::PaymentMethodType::Walley => Self::PayLater,
            api_enums::PaymentMethodType::Giropay
            | api_enums::PaymentMethodType::Ideal
            | api_enums::PaymentMethodType::Sofort
            | api_enums::PaymentMethodType::Eft
            | api_enums::PaymentMethodType::Eps
            | api_enums::PaymentMethodType::BancontactCard
            | api_enums::PaymentMethodType::Blik
            | api_enums::PaymentMethodType::LocalBankRedirect
            | api_enums::PaymentMethodType::OnlineBankingThailand
            | api_enums::PaymentMethodType::OnlineBankingCzechRepublic
            | api_enums::PaymentMethodType::OnlineBankingFinland
            | api_enums::PaymentMethodType::OnlineBankingFpx
            | api_enums::PaymentMethodType::OnlineBankingPoland
            | api_enums::PaymentMethodType::OnlineBankingSlovakia
            | api_enums::PaymentMethodType::OpenBankingUk
            | api_enums::PaymentMethodType::OpenBankingPIS
            | api_enums::PaymentMethodType::Przelewy24
            | api_enums::PaymentMethodType::Trustly
            | api_enums::PaymentMethodType::Bizum
            | api_enums::PaymentMethodType::Interac => Self::BankRedirect,
            api_enums::PaymentMethodType::UpiCollect | api_enums::PaymentMethodType::UpiIntent => {
                Self::Upi
            }
            api_enums::PaymentMethodType::CryptoCurrency => Self::Crypto,
            api_enums::PaymentMethodType::Ach
            | api_enums::PaymentMethodType::Sepa
            | api_enums::PaymentMethodType::Bacs
            | api_enums::PaymentMethodType::Becs => Self::BankDebit,
            api_enums::PaymentMethodType::Credit | api_enums::PaymentMethodType::Debit => {
                Self::Card
            }
            #[cfg(feature = "v2")]
            api_enums::PaymentMethodType::Card => Self::Card,
            api_enums::PaymentMethodType::Evoucher
            | api_enums::PaymentMethodType::ClassicReward => Self::Reward,
            api_enums::PaymentMethodType::Boleto
            | api_enums::PaymentMethodType::Efecty
            | api_enums::PaymentMethodType::PagoEfectivo
            | api_enums::PaymentMethodType::RedCompra
            | api_enums::PaymentMethodType::Alfamart
            | api_enums::PaymentMethodType::Indomaret
            | api_enums::PaymentMethodType::Oxxo
            | api_enums::PaymentMethodType::SevenEleven
            | api_enums::PaymentMethodType::Lawson
            | api_enums::PaymentMethodType::MiniStop
            | api_enums::PaymentMethodType::FamilyMart
            | api_enums::PaymentMethodType::Seicomart
            | api_enums::PaymentMethodType::PayEasy
            | api_enums::PaymentMethodType::RedPagos => Self::Voucher,
            api_enums::PaymentMethodType::Pse
            | api_enums::PaymentMethodType::Multibanco
            | api_enums::PaymentMethodType::PermataBankTransfer
            | api_enums::PaymentMethodType::BcaBankTransfer
            | api_enums::PaymentMethodType::BniVa
            | api_enums::PaymentMethodType::BriVa
            | api_enums::PaymentMethodType::CimbVa
            | api_enums::PaymentMethodType::DanamonVa
            | api_enums::PaymentMethodType::MandiriVa
            | api_enums::PaymentMethodType::LocalBankTransfer
            | api_enums::PaymentMethodType::InstantBankTransfer
            | api_enums::PaymentMethodType::SepaBankTransfer
            | api_enums::PaymentMethodType::Pix => Self::BankTransfer,
            api_enums::PaymentMethodType::Givex | api_enums::PaymentMethodType::PaySafeCard => {
                Self::GiftCard
            }
            api_enums::PaymentMethodType::Benefit
            | api_enums::PaymentMethodType::Knet
            | api_enums::PaymentMethodType::MomoAtm
            | api_enums::PaymentMethodType::CardRedirect => Self::CardRedirect,
            api_enums::PaymentMethodType::Fps
            | api_enums::PaymentMethodType::DuitNow
            | api_enums::PaymentMethodType::PromptPay
            | api_enums::PaymentMethodType::VietQr => Self::RealTimePayment,
            api_enums::PaymentMethodType::DirectCarrierBilling => Self::MobilePayment,
        }
    }
}

impl ForeignTryFrom<payments::PaymentMethodData> for api_enums::PaymentMethod {
    type Error = errors::ApiErrorResponse;
    fn foreign_try_from(
        payment_method_data: payments::PaymentMethodData,
    ) -> Result<Self, Self::Error> {
        match payment_method_data {
            payments::PaymentMethodData::Card(..) | payments::PaymentMethodData::CardToken(..) => {
                Ok(Self::Card)
            }
            payments::PaymentMethodData::Wallet(..) => Ok(Self::Wallet),
            payments::PaymentMethodData::PayLater(..) => Ok(Self::PayLater),
            payments::PaymentMethodData::BankRedirect(..) => Ok(Self::BankRedirect),
            payments::PaymentMethodData::BankDebit(..) => Ok(Self::BankDebit),
            payments::PaymentMethodData::BankTransfer(..) => Ok(Self::BankTransfer),
            payments::PaymentMethodData::Crypto(..) => Ok(Self::Crypto),
            payments::PaymentMethodData::Reward => Ok(Self::Reward),
            payments::PaymentMethodData::RealTimePayment(..) => Ok(Self::RealTimePayment),
            payments::PaymentMethodData::Upi(..) => Ok(Self::Upi),
            payments::PaymentMethodData::Voucher(..) => Ok(Self::Voucher),
            payments::PaymentMethodData::GiftCard(..) => Ok(Self::GiftCard),
            payments::PaymentMethodData::CardRedirect(..) => Ok(Self::CardRedirect),
            payments::PaymentMethodData::OpenBanking(..) => Ok(Self::OpenBanking),
            payments::PaymentMethodData::MobilePayment(..) => Ok(Self::MobilePayment),
            payments::PaymentMethodData::MandatePayment => {
                Err(errors::ApiErrorResponse::InvalidRequestData {
                    message: ("Mandate payments cannot have payment_method_data field".to_string()),
                })
            }
        }
    }
}

impl ForeignFrom<storage_enums::RefundStatus> for Option<storage_enums::EventType> {
    fn foreign_from(value: storage_enums::RefundStatus) -> Self {
        match value {
            storage_enums::RefundStatus::Success => Some(storage_enums::EventType::RefundSucceeded),
            storage_enums::RefundStatus::Failure => Some(storage_enums::EventType::RefundFailed),
            api_enums::RefundStatus::ManualReview
            | api_enums::RefundStatus::Pending
            | api_enums::RefundStatus::TransactionFailure => None,
        }
    }
}

impl ForeignFrom<storage_enums::PayoutStatus> for Option<storage_enums::EventType> {
    fn foreign_from(value: storage_enums::PayoutStatus) -> Self {
        match value {
            storage_enums::PayoutStatus::Success => Some(storage_enums::EventType::PayoutSuccess),
            storage_enums::PayoutStatus::Failed => Some(storage_enums::EventType::PayoutFailed),
            storage_enums::PayoutStatus::Cancelled => {
                Some(storage_enums::EventType::PayoutCancelled)
            }
            storage_enums::PayoutStatus::Initiated => {
                Some(storage_enums::EventType::PayoutInitiated)
            }
            storage_enums::PayoutStatus::Expired => Some(storage_enums::EventType::PayoutExpired),
            storage_enums::PayoutStatus::Reversed => Some(storage_enums::EventType::PayoutReversed),
            storage_enums::PayoutStatus::Ineligible
            | storage_enums::PayoutStatus::Pending
            | storage_enums::PayoutStatus::RequiresCreation
            | storage_enums::PayoutStatus::RequiresFulfillment
            | storage_enums::PayoutStatus::RequiresPayoutMethodData
            | storage_enums::PayoutStatus::RequiresVendorAccountCreation
            | storage_enums::PayoutStatus::RequiresConfirmation => None,
        }
    }
}

impl ForeignFrom<storage_enums::DisputeStatus> for storage_enums::EventType {
    fn foreign_from(value: storage_enums::DisputeStatus) -> Self {
        match value {
            storage_enums::DisputeStatus::DisputeOpened => Self::DisputeOpened,
            storage_enums::DisputeStatus::DisputeExpired => Self::DisputeExpired,
            storage_enums::DisputeStatus::DisputeAccepted => Self::DisputeAccepted,
            storage_enums::DisputeStatus::DisputeCancelled => Self::DisputeCancelled,
            storage_enums::DisputeStatus::DisputeChallenged => Self::DisputeChallenged,
            storage_enums::DisputeStatus::DisputeWon => Self::DisputeWon,
            storage_enums::DisputeStatus::DisputeLost => Self::DisputeLost,
        }
    }
}

impl ForeignFrom<storage_enums::MandateStatus> for Option<storage_enums::EventType> {
    fn foreign_from(value: storage_enums::MandateStatus) -> Self {
        match value {
            storage_enums::MandateStatus::Active => Some(storage_enums::EventType::MandateActive),
            storage_enums::MandateStatus::Revoked => Some(storage_enums::EventType::MandateRevoked),
            storage_enums::MandateStatus::Inactive | storage_enums::MandateStatus::Pending => None,
        }
    }
}

impl ForeignTryFrom<api_models::webhooks::IncomingWebhookEvent> for storage_enums::RefundStatus {
    type Error = errors::ValidationError;

    fn foreign_try_from(
        value: api_models::webhooks::IncomingWebhookEvent,
    ) -> Result<Self, Self::Error> {
        match value {
            api_models::webhooks::IncomingWebhookEvent::RefundSuccess => Ok(Self::Success),
            api_models::webhooks::IncomingWebhookEvent::RefundFailure => Ok(Self::Failure),
            _ => Err(errors::ValidationError::IncorrectValueProvided {
                field_name: "incoming_webhook_event_type",
            }),
        }
    }
}

impl ForeignTryFrom<api_models::webhooks::IncomingWebhookEvent> for api_enums::RelayStatus {
    type Error = errors::ValidationError;

    fn foreign_try_from(
        value: api_models::webhooks::IncomingWebhookEvent,
    ) -> Result<Self, Self::Error> {
        match value {
            api_models::webhooks::IncomingWebhookEvent::RefundSuccess => Ok(Self::Success),
            api_models::webhooks::IncomingWebhookEvent::RefundFailure => Ok(Self::Failure),
            _ => Err(errors::ValidationError::IncorrectValueProvided {
                field_name: "incoming_webhook_event_type",
            }),
        }
    }
}

#[cfg(feature = "payouts")]
impl ForeignTryFrom<api_models::webhooks::IncomingWebhookEvent> for storage_enums::PayoutStatus {
    type Error = errors::ValidationError;

    fn foreign_try_from(
        value: api_models::webhooks::IncomingWebhookEvent,
    ) -> Result<Self, Self::Error> {
        match value {
            api_models::webhooks::IncomingWebhookEvent::PayoutSuccess => Ok(Self::Success),
            api_models::webhooks::IncomingWebhookEvent::PayoutFailure => Ok(Self::Failed),
            api_models::webhooks::IncomingWebhookEvent::PayoutCancelled => Ok(Self::Cancelled),
            api_models::webhooks::IncomingWebhookEvent::PayoutProcessing => Ok(Self::Pending),
            api_models::webhooks::IncomingWebhookEvent::PayoutCreated => Ok(Self::Initiated),
            api_models::webhooks::IncomingWebhookEvent::PayoutExpired => Ok(Self::Expired),
            api_models::webhooks::IncomingWebhookEvent::PayoutReversed => Ok(Self::Reversed),
            _ => Err(errors::ValidationError::IncorrectValueProvided {
                field_name: "incoming_webhook_event_type",
            }),
        }
    }
}

impl ForeignTryFrom<api_models::webhooks::IncomingWebhookEvent> for storage_enums::MandateStatus {
    type Error = errors::ValidationError;

    fn foreign_try_from(
        value: api_models::webhooks::IncomingWebhookEvent,
    ) -> Result<Self, Self::Error> {
        match value {
            api_models::webhooks::IncomingWebhookEvent::MandateActive => Ok(Self::Active),
            api_models::webhooks::IncomingWebhookEvent::MandateRevoked => Ok(Self::Revoked),
            _ => Err(errors::ValidationError::IncorrectValueProvided {
                field_name: "incoming_webhook_event_type",
            }),
        }
    }
}

impl ForeignFrom<storage::Config> for api_types::Config {
    fn foreign_from(config: storage::Config) -> Self {
        Self {
            key: config.key,
            value: config.config,
        }
    }
}

impl ForeignFrom<&api_types::ConfigUpdate> for storage::ConfigUpdate {
    fn foreign_from(config: &api_types::ConfigUpdate) -> Self {
        Self::Update {
            config: Some(config.value.clone()),
        }
    }
}

impl From<&domain::Address> for hyperswitch_domain_models::address::Address {
    fn from(address: &domain::Address) -> Self {
        // If all the fields of address are none, then pass the address as None
        let address_details = if address.city.is_none()
            && address.line1.is_none()
            && address.line2.is_none()
            && address.line3.is_none()
            && address.state.is_none()
            && address.country.is_none()
            && address.zip.is_none()
            && address.first_name.is_none()
            && address.last_name.is_none()
        {
            None
        } else {
            Some(hyperswitch_domain_models::address::AddressDetails {
                city: address.city.clone(),
                country: address.country,
                line1: address.line1.clone().map(Encryptable::into_inner),
                line2: address.line2.clone().map(Encryptable::into_inner),
                line3: address.line3.clone().map(Encryptable::into_inner),
                state: address.state.clone().map(Encryptable::into_inner),
                zip: address.zip.clone().map(Encryptable::into_inner),
                first_name: address.first_name.clone().map(Encryptable::into_inner),
                last_name: address.last_name.clone().map(Encryptable::into_inner),
            })
        };

        // If all the fields of phone are none, then pass the phone as None
        let phone_details = if address.phone_number.is_none() && address.country_code.is_none() {
            None
        } else {
            Some(hyperswitch_domain_models::address::PhoneDetails {
                number: address.phone_number.clone().map(Encryptable::into_inner),
                country_code: address.country_code.clone(),
            })
        };

        Self {
            address: address_details,
            phone: phone_details,
            email: address.email.clone().map(pii::Email::from),
        }
    }
}

impl ForeignFrom<domain::Address> for api_types::Address {
    fn foreign_from(address: domain::Address) -> Self {
        // If all the fields of address are none, then pass the address as None
        let address_details = if address.city.is_none()
            && address.line1.is_none()
            && address.line2.is_none()
            && address.line3.is_none()
            && address.state.is_none()
            && address.country.is_none()
            && address.zip.is_none()
            && address.first_name.is_none()
            && address.last_name.is_none()
        {
            None
        } else {
            Some(api_types::AddressDetails {
                city: address.city.clone(),
                country: address.country,
                line1: address.line1.clone().map(Encryptable::into_inner),
                line2: address.line2.clone().map(Encryptable::into_inner),
                line3: address.line3.clone().map(Encryptable::into_inner),
                state: address.state.clone().map(Encryptable::into_inner),
                zip: address.zip.clone().map(Encryptable::into_inner),
                first_name: address.first_name.clone().map(Encryptable::into_inner),
                last_name: address.last_name.clone().map(Encryptable::into_inner),
            })
        };

        // If all the fields of phone are none, then pass the phone as None
        let phone_details = if address.phone_number.is_none() && address.country_code.is_none() {
            None
        } else {
            Some(api_types::PhoneDetails {
                number: address.phone_number.clone().map(Encryptable::into_inner),
                country_code: address.country_code.clone(),
            })
        };

        Self {
            address: address_details,
            phone: phone_details,
            email: address.email.clone().map(pii::Email::from),
        }
    }
}

impl
    ForeignFrom<(
        diesel_models::api_keys::ApiKey,
        crate::core::api_keys::PlaintextApiKey,
    )> for api_models::api_keys::CreateApiKeyResponse
{
    fn foreign_from(
        item: (
            diesel_models::api_keys::ApiKey,
            crate::core::api_keys::PlaintextApiKey,
        ),
    ) -> Self {
        use masking::StrongSecret;

        let (api_key, plaintext_api_key) = item;
        Self {
            key_id: api_key.key_id,
            merchant_id: api_key.merchant_id,
            name: api_key.name,
            description: api_key.description,
            api_key: StrongSecret::from(plaintext_api_key.peek().to_owned()),
            created: api_key.created_at,
            expiration: api_key.expires_at.into(),
        }
    }
}

impl ForeignFrom<diesel_models::api_keys::ApiKey> for api_models::api_keys::RetrieveApiKeyResponse {
    fn foreign_from(api_key: diesel_models::api_keys::ApiKey) -> Self {
        Self {
            key_id: api_key.key_id,
            merchant_id: api_key.merchant_id,
            name: api_key.name,
            description: api_key.description,
            prefix: api_key.prefix.into(),
            created: api_key.created_at,
            expiration: api_key.expires_at.into(),
        }
    }
}

impl ForeignFrom<api_models::api_keys::UpdateApiKeyRequest>
    for diesel_models::api_keys::ApiKeyUpdate
{
    fn foreign_from(api_key: api_models::api_keys::UpdateApiKeyRequest) -> Self {
        Self::Update {
            name: api_key.name,
            description: api_key.description,
            expires_at: api_key.expiration.map(Into::into),
            last_used: None,
        }
    }
}

impl ForeignTryFrom<api_models::webhooks::IncomingWebhookEvent> for storage_enums::DisputeStatus {
    type Error = errors::ValidationError;

    fn foreign_try_from(
        value: api_models::webhooks::IncomingWebhookEvent,
    ) -> Result<Self, Self::Error> {
        match value {
            api_models::webhooks::IncomingWebhookEvent::DisputeOpened => Ok(Self::DisputeOpened),
            api_models::webhooks::IncomingWebhookEvent::DisputeExpired => Ok(Self::DisputeExpired),
            api_models::webhooks::IncomingWebhookEvent::DisputeAccepted => {
                Ok(Self::DisputeAccepted)
            }
            api_models::webhooks::IncomingWebhookEvent::DisputeCancelled => {
                Ok(Self::DisputeCancelled)
            }
            api_models::webhooks::IncomingWebhookEvent::DisputeChallenged => {
                Ok(Self::DisputeChallenged)
            }
            api_models::webhooks::IncomingWebhookEvent::DisputeWon => Ok(Self::DisputeWon),
            api_models::webhooks::IncomingWebhookEvent::DisputeLost => Ok(Self::DisputeLost),
            _ => Err(errors::ValidationError::IncorrectValueProvided {
                field_name: "incoming_webhook_event",
            }),
        }
    }
}

impl ForeignFrom<storage::Dispute> for api_models::disputes::DisputeResponse {
    fn foreign_from(dispute: storage::Dispute) -> Self {
        Self {
            dispute_id: dispute.dispute_id,
            payment_id: dispute.payment_id,
            attempt_id: dispute.attempt_id,
            amount: dispute.amount,
            currency: dispute.dispute_currency.unwrap_or(
                dispute
                    .currency
                    .to_uppercase()
                    .parse_enum("Currency")
                    .unwrap_or_default(),
            ),
            dispute_stage: dispute.dispute_stage,
            dispute_status: dispute.dispute_status,
            connector: dispute.connector,
            connector_status: dispute.connector_status,
            connector_dispute_id: dispute.connector_dispute_id,
            connector_reason: dispute.connector_reason,
            connector_reason_code: dispute.connector_reason_code,
            challenge_required_by: dispute.challenge_required_by,
            connector_created_at: dispute.connector_created_at,
            connector_updated_at: dispute.connector_updated_at,
            created_at: dispute.created_at,
            profile_id: dispute.profile_id,
            merchant_connector_id: dispute.merchant_connector_id,
        }
    }
}

impl ForeignFrom<storage::Authorization> for payments::IncrementalAuthorizationResponse {
    fn foreign_from(authorization: storage::Authorization) -> Self {
        Self {
            authorization_id: authorization.authorization_id,
            amount: authorization.amount,
            status: authorization.status,
            error_code: authorization.error_code,
            error_message: authorization.error_message,
            previously_authorized_amount: authorization.previously_authorized_amount,
        }
    }
}

impl
    ForeignFrom<
        &hyperswitch_domain_models::router_request_types::authentication::AuthenticationStore,
    > for payments::ExternalAuthenticationDetailsResponse
{
    fn foreign_from(
        authn_store: &hyperswitch_domain_models::router_request_types::authentication::AuthenticationStore,
    ) -> Self {
        let authn_data = &authn_store.authentication;
        let version = authn_data
            .maximum_supported_version
            .as_ref()
            .map(|version| version.to_string());
        Self {
            authentication_flow: authn_data.authentication_type,
            electronic_commerce_indicator: authn_data.eci.clone(),
            status: authn_data.authentication_status,
            ds_transaction_id: authn_data.threeds_server_transaction_id.clone(),
            version,
            error_code: authn_data.error_code.clone(),
            error_message: authn_data.error_message.clone(),
        }
    }
}

impl ForeignFrom<storage::Dispute> for api_models::disputes::DisputeResponsePaymentsRetrieve {
    fn foreign_from(dispute: storage::Dispute) -> Self {
        Self {
            dispute_id: dispute.dispute_id,
            dispute_stage: dispute.dispute_stage,
            dispute_status: dispute.dispute_status,
            connector_status: dispute.connector_status,
            connector_dispute_id: dispute.connector_dispute_id,
            connector_reason: dispute.connector_reason,
            connector_reason_code: dispute.connector_reason_code,
            challenge_required_by: dispute.challenge_required_by,
            connector_created_at: dispute.connector_created_at,
            connector_updated_at: dispute.connector_updated_at,
            created_at: dispute.created_at,
        }
    }
}

impl ForeignFrom<storage::FileMetadata> for api_models::files::FileMetadataResponse {
    fn foreign_from(file_metadata: storage::FileMetadata) -> Self {
        Self {
            file_id: file_metadata.file_id,
            file_name: file_metadata.file_name,
            file_size: file_metadata.file_size,
            file_type: file_metadata.file_type,
            available: file_metadata.available,
        }
    }
}

impl ForeignFrom<diesel_models::cards_info::CardInfo> for api_models::cards_info::CardInfoResponse {
    fn foreign_from(item: diesel_models::cards_info::CardInfo) -> Self {
        Self {
            card_iin: item.card_iin,
            card_type: item.card_type,
            card_sub_type: item.card_subtype,
            card_network: item.card_network.map(|x| x.to_string()),
            card_issuer: item.card_issuer,
            card_issuing_country: item.card_issuing_country,
        }
    }
}

impl ForeignTryFrom<domain::MerchantConnectorAccount>
    for api_models::admin::MerchantConnectorListResponse
{
    type Error = error_stack::Report<errors::ApiErrorResponse>;
    fn foreign_try_from(item: domain::MerchantConnectorAccount) -> Result<Self, Self::Error> {
        #[cfg(feature = "v1")]
        let payment_methods_enabled = match item.payment_methods_enabled {
            Some(secret_val) => {
                let val = secret_val
                    .into_iter()
                    .map(|secret| secret.expose())
                    .collect();
                serde_json::Value::Array(val)
                    .parse_value("PaymentMethods")
                    .change_context(errors::ApiErrorResponse::InternalServerError)?
            }
            None => None,
        };

        let frm_configs = match item.frm_configs {
            Some(frm_value) => {
                let configs_for_frm : Vec<api_models::admin::FrmConfigs> = frm_value
                    .iter()
                    .map(|config| { config
                        .peek()
                        .clone()
                        .parse_value("FrmConfigs")
                        .change_context(errors::ApiErrorResponse::InvalidDataFormat {
                            field_name: "frm_configs".to_string(),
                            expected_format: r#"[{ "gateway": "stripe", "payment_methods": [{ "payment_method": "card","payment_method_types": [{"payment_method_type": "credit","card_networks": ["Visa"],"flow": "pre","action": "cancel_txn"}]}]}]"#.to_string(),
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Some(configs_for_frm)
            }
            None => None,
        };
        #[cfg(feature = "v1")]
        let response = Self {
            connector_type: item.connector_type,
            connector_name: item.connector_name,
            connector_label: item.connector_label,
            merchant_connector_id: item.merchant_connector_id,
            test_mode: item.test_mode,
            disabled: item.disabled,
            payment_methods_enabled,
            business_country: item.business_country,
            business_label: item.business_label,
            business_sub_label: item.business_sub_label,
            frm_configs,
            profile_id: item.profile_id,
            applepay_verified_domains: item.applepay_verified_domains,
            pm_auth_config: item.pm_auth_config,
            status: item.status,
        };
        #[cfg(feature = "v2")]
        let response = Self {
            id: item.id,
            connector_type: item.connector_type,
            connector_name: item.connector_name,
            connector_label: item.connector_label,
            disabled: item.disabled,
            payment_methods_enabled: item.payment_methods_enabled,
            frm_configs,
            profile_id: item.profile_id,
            applepay_verified_domains: item.applepay_verified_domains,
            pm_auth_config: item.pm_auth_config,
            status: item.status,
        };
        Ok(response)
    }
}

#[cfg(feature = "v1")]
impl ForeignTryFrom<domain::MerchantConnectorAccount>
    for api_models::admin::MerchantConnectorResponse
{
    type Error = error_stack::Report<errors::ApiErrorResponse>;
    fn foreign_try_from(item: domain::MerchantConnectorAccount) -> Result<Self, Self::Error> {
        let payment_methods_enabled = match item.payment_methods_enabled.clone() {
            Some(secret_val) => {
                let val = secret_val
                    .into_iter()
                    .map(|secret| secret.expose())
                    .collect();
                serde_json::Value::Array(val)
                    .parse_value("PaymentMethods")
                    .change_context(errors::ApiErrorResponse::InternalServerError)?
            }
            None => None,
        };
        let frm_configs = match item.frm_configs {
            Some(ref frm_value) => {
                let configs_for_frm : Vec<api_models::admin::FrmConfigs> = frm_value
                    .iter()
                    .map(|config| { config
                        .peek()
                        .clone()
                        .parse_value("FrmConfigs")
                        .change_context(errors::ApiErrorResponse::InvalidDataFormat {
                            field_name: "frm_configs".to_string(),
                            expected_format: r#"[{ "gateway": "stripe", "payment_methods": [{ "payment_method": "card","payment_method_types": [{"payment_method_type": "credit","card_networks": ["Visa"],"flow": "pre","action": "cancel_txn"}]}]}]"#.to_string(),
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Some(configs_for_frm)
            }
            None => None,
        };
        // parse the connector_account_details into ConnectorAuthType
        let connector_account_details: hyperswitch_domain_models::router_data::ConnectorAuthType =
            item.connector_account_details
                .clone()
                .into_inner()
                .parse_value("ConnectorAuthType")
                .change_context(errors::ApiErrorResponse::InternalServerError)
                .attach_printable("Failed while parsing value for ConnectorAuthType")?;
        // get the masked keys from the ConnectorAuthType and encode it to secret value
        let masked_connector_account_details = Secret::new(
            connector_account_details
                .get_masked_keys()
                .encode_to_value()
                .change_context(errors::ApiErrorResponse::InternalServerError)
                .attach_printable("Failed to encode ConnectorAuthType")?,
        );
        #[cfg(feature = "v2")]
        let response = Self {
            id: item.get_id(),
            connector_type: item.connector_type,
            connector_name: item.connector_name,
            connector_label: item.connector_label,
            connector_account_details: masked_connector_account_details,
            disabled: item.disabled,
            payment_methods_enabled,
            metadata: item.metadata,
            frm_configs,
            connector_webhook_details: item
                .connector_webhook_details
                .map(|webhook_details| {
                    serde_json::Value::parse_value(
                        webhook_details.expose(),
                        "MerchantConnectorWebhookDetails",
                    )
                    .attach_printable("Unable to deserialize connector_webhook_details")
                    .change_context(errors::ApiErrorResponse::InternalServerError)
                })
                .transpose()?,
            profile_id: item.profile_id,
            applepay_verified_domains: item.applepay_verified_domains,
            pm_auth_config: item.pm_auth_config,
            status: item.status,
            additional_merchant_data: item
                .additional_merchant_data
                .map(|data| {
                    let data = data.into_inner();
                    serde_json::Value::parse_value::<router_types::AdditionalMerchantData>(
                        data.expose(),
                        "AdditionalMerchantData",
                    )
                    .attach_printable("Unable to deserialize additional_merchant_data")
                    .change_context(errors::ApiErrorResponse::InternalServerError)
                })
                .transpose()?
                .map(api_models::admin::AdditionalMerchantData::foreign_from),
            connector_wallets_details: item
                .connector_wallets_details
                .map(|data| {
                    data.into_inner()
                        .expose()
                        .parse_value::<api_models::admin::ConnectorWalletDetails>(
                            "ConnectorWalletDetails",
                        )
                        .attach_printable("Unable to deserialize connector_wallets_details")
                        .change_context(errors::ApiErrorResponse::InternalServerError)
                })
                .transpose()?,
        };
        #[cfg(feature = "v1")]
        let response = Self {
            connector_type: item.connector_type,
            connector_name: item.connector_name,
            connector_label: item.connector_label,
            merchant_connector_id: item.merchant_connector_id,
            connector_account_details: masked_connector_account_details,
            test_mode: item.test_mode,
            disabled: item.disabled,
            payment_methods_enabled,
            metadata: item.metadata,
            business_country: item.business_country,
            business_label: item.business_label,
            business_sub_label: item.business_sub_label,
            frm_configs,
            connector_webhook_details: item
                .connector_webhook_details
                .map(|webhook_details| {
                    serde_json::Value::parse_value(
                        webhook_details.expose(),
                        "MerchantConnectorWebhookDetails",
                    )
                    .attach_printable("Unable to deserialize connector_webhook_details")
                    .change_context(errors::ApiErrorResponse::InternalServerError)
                })
                .transpose()?,
            profile_id: item.profile_id,
            applepay_verified_domains: item.applepay_verified_domains,
            pm_auth_config: item.pm_auth_config,
            status: item.status,
            additional_merchant_data: item
                .additional_merchant_data
                .map(|data| {
                    let data = data.into_inner();
                    serde_json::Value::parse_value::<router_types::AdditionalMerchantData>(
                        data.expose(),
                        "AdditionalMerchantData",
                    )
                    .attach_printable("Unable to deserialize additional_merchant_data")
                    .change_context(errors::ApiErrorResponse::InternalServerError)
                })
                .transpose()?
                .map(api_models::admin::AdditionalMerchantData::foreign_from),
            connector_wallets_details: item
                .connector_wallets_details
                .map(|data| {
                    data.into_inner()
                        .expose()
                        .parse_value::<api_models::admin::ConnectorWalletDetails>(
                            "ConnectorWalletDetails",
                        )
                        .attach_printable("Unable to deserialize connector_wallets_details")
                        .change_context(errors::ApiErrorResponse::InternalServerError)
                })
                .transpose()?,
        };
        Ok(response)
    }
}

#[cfg(feature = "v2")]
impl ForeignTryFrom<domain::MerchantConnectorAccount>
    for api_models::admin::MerchantConnectorResponse
{
    type Error = error_stack::Report<errors::ApiErrorResponse>;
    fn foreign_try_from(item: domain::MerchantConnectorAccount) -> Result<Self, Self::Error> {
        let frm_configs = match item.frm_configs {
            Some(ref frm_value) => {
                let configs_for_frm : Vec<api_models::admin::FrmConfigs> = frm_value
                    .iter()
                    .map(|config| { config
                        .peek()
                        .clone()
                        .parse_value("FrmConfigs")
                        .change_context(errors::ApiErrorResponse::InvalidDataFormat {
                            field_name: "frm_configs".to_string(),
                            expected_format: r#"[{ "gateway": "stripe", "payment_methods": [{ "payment_method": "card","payment_method_types": [{"payment_method_type": "credit","card_networks": ["Visa"],"flow": "pre","action": "cancel_txn"}]}]}]"#.to_string(),
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Some(configs_for_frm)
            }
            None => None,
        };

        // parse the connector_account_details into ConnectorAuthType
        let connector_account_details: hyperswitch_domain_models::router_data::ConnectorAuthType =
            item.connector_account_details
                .clone()
                .into_inner()
                .parse_value("ConnectorAuthType")
                .change_context(errors::ApiErrorResponse::InternalServerError)
                .attach_printable("Failed while parsing value for ConnectorAuthType")?;
        // get the masked keys from the ConnectorAuthType and encode it to secret value
        let masked_connector_account_details = Secret::new(
            connector_account_details
                .get_masked_keys()
                .encode_to_value()
                .change_context(errors::ApiErrorResponse::InternalServerError)
                .attach_printable("Failed to encode ConnectorAuthType")?,
        );

        let feature_metadata = item.feature_metadata.as_ref().map(|metadata| {
            api_models::admin::MerchantConnectorAccountFeatureMetadata::foreign_from(metadata)
        });

        let response = Self {
            id: item.get_id(),
            connector_type: item.connector_type,
            connector_name: item.connector_name,
            connector_label: item.connector_label,
            connector_account_details: masked_connector_account_details,
            disabled: item.disabled,
            payment_methods_enabled: item.payment_methods_enabled,
            metadata: item.metadata,
            frm_configs,
            connector_webhook_details: item
                .connector_webhook_details
                .map(|webhook_details| {
                    serde_json::Value::parse_value(
                        webhook_details.expose(),
                        "MerchantConnectorWebhookDetails",
                    )
                    .attach_printable("Unable to deserialize connector_webhook_details")
                    .change_context(errors::ApiErrorResponse::InternalServerError)
                })
                .transpose()?,
            profile_id: item.profile_id,
            applepay_verified_domains: item.applepay_verified_domains,
            pm_auth_config: item.pm_auth_config,
            status: item.status,
            additional_merchant_data: item
                .additional_merchant_data
                .map(|data| {
                    let data = data.into_inner();
                    serde_json::Value::parse_value::<router_types::AdditionalMerchantData>(
                        data.expose(),
                        "AdditionalMerchantData",
                    )
                    .attach_printable("Unable to deserialize additional_merchant_data")
                    .change_context(errors::ApiErrorResponse::InternalServerError)
                })
                .transpose()?
                .map(api_models::admin::AdditionalMerchantData::foreign_from),
            connector_wallets_details: item
                .connector_wallets_details
                .map(|data| {
                    data.into_inner()
                        .expose()
                        .parse_value::<api_models::admin::ConnectorWalletDetails>(
                            "ConnectorWalletDetails",
                        )
                        .attach_printable("Unable to deserialize connector_wallets_details")
                        .change_context(errors::ApiErrorResponse::InternalServerError)
                })
                .transpose()?,
            feature_metadata,
        };
        Ok(response)
    }
}

#[cfg(feature = "v1")]
impl ForeignFrom<storage::PaymentAttempt> for payments::PaymentAttemptResponse {
    fn foreign_from(payment_attempt: storage::PaymentAttempt) -> Self {
        let connector_transaction_id = payment_attempt
            .get_connector_payment_id()
            .map(ToString::to_string);
        Self {
            attempt_id: payment_attempt.attempt_id,
            status: payment_attempt.status,
            amount: payment_attempt.net_amount.get_order_amount(),
            order_tax_amount: payment_attempt.net_amount.get_order_tax_amount(),
            currency: payment_attempt.currency,
            connector: payment_attempt.connector,
            error_message: payment_attempt.error_reason,
            payment_method: payment_attempt.payment_method,
            connector_transaction_id,
            capture_method: payment_attempt.capture_method,
            authentication_type: payment_attempt.authentication_type,
            created_at: payment_attempt.created_at,
            modified_at: payment_attempt.modified_at,
            cancellation_reason: payment_attempt.cancellation_reason,
            mandate_id: payment_attempt.mandate_id,
            error_code: payment_attempt.error_code,
            payment_token: payment_attempt.payment_token,
            connector_metadata: payment_attempt.connector_metadata,
            payment_experience: payment_attempt.payment_experience,
            payment_method_type: payment_attempt.payment_method_type,
            reference_id: payment_attempt.connector_response_reference_id,
            unified_code: payment_attempt.unified_code,
            unified_message: payment_attempt.unified_message,
            client_source: payment_attempt.client_source,
            client_version: payment_attempt.client_version,
        }
    }
}

impl ForeignFrom<storage::Capture> for payments::CaptureResponse {
    fn foreign_from(capture: storage::Capture) -> Self {
        let connector_capture_id = capture.get_optional_connector_transaction_id().cloned();
        Self {
            capture_id: capture.capture_id,
            status: capture.status,
            amount: capture.amount,
            currency: capture.currency,
            connector: capture.connector,
            authorized_attempt_id: capture.authorized_attempt_id,
            connector_capture_id,
            capture_sequence: capture.capture_sequence,
            error_message: capture.error_message,
            error_code: capture.error_code,
            error_reason: capture.error_reason,
            reference_id: capture.connector_response_reference_id,
        }
    }
}

#[cfg(feature = "payouts")]
impl ForeignFrom<&api_models::payouts::PayoutMethodData> for api_enums::PaymentMethodType {
    fn foreign_from(value: &api_models::payouts::PayoutMethodData) -> Self {
        match value {
            api_models::payouts::PayoutMethodData::Bank(bank) => Self::foreign_from(bank),
            api_models::payouts::PayoutMethodData::Card(_) => Self::Debit,
            api_models::payouts::PayoutMethodData::Wallet(wallet) => Self::foreign_from(wallet),
        }
    }
}

#[cfg(feature = "payouts")]
impl ForeignFrom<&api_models::payouts::Bank> for api_enums::PaymentMethodType {
    fn foreign_from(value: &api_models::payouts::Bank) -> Self {
        match value {
            api_models::payouts::Bank::Ach(_) => Self::Ach,
            api_models::payouts::Bank::Bacs(_) => Self::Bacs,
            api_models::payouts::Bank::Sepa(_) => Self::SepaBankTransfer,
            api_models::payouts::Bank::Pix(_) => Self::Pix,
        }
    }
}

#[cfg(feature = "payouts")]
impl ForeignFrom<&api_models::payouts::Wallet> for api_enums::PaymentMethodType {
    fn foreign_from(value: &api_models::payouts::Wallet) -> Self {
        match value {
            api_models::payouts::Wallet::Paypal(_) => Self::Paypal,
            api_models::payouts::Wallet::Venmo(_) => Self::Venmo,
        }
    }
}

#[cfg(feature = "payouts")]
impl ForeignFrom<&api_models::payouts::PayoutMethodData> for api_enums::PaymentMethod {
    fn foreign_from(value: &api_models::payouts::PayoutMethodData) -> Self {
        match value {
            api_models::payouts::PayoutMethodData::Bank(_) => Self::BankTransfer,
            api_models::payouts::PayoutMethodData::Card(_) => Self::Card,
            api_models::payouts::PayoutMethodData::Wallet(_) => Self::Wallet,
        }
    }
}

#[cfg(feature = "payouts")]
impl ForeignFrom<&api_models::payouts::PayoutMethodData> for api_models::enums::PayoutType {
    fn foreign_from(value: &api_models::payouts::PayoutMethodData) -> Self {
        match value {
            api_models::payouts::PayoutMethodData::Bank(_) => Self::Bank,
            api_models::payouts::PayoutMethodData::Card(_) => Self::Card,
            api_models::payouts::PayoutMethodData::Wallet(_) => Self::Wallet,
        }
    }
}

#[cfg(feature = "payouts")]
impl ForeignFrom<api_models::enums::PayoutType> for api_enums::PaymentMethod {
    fn foreign_from(value: api_models::enums::PayoutType) -> Self {
        match value {
            api_models::enums::PayoutType::Bank => Self::BankTransfer,
            api_models::enums::PayoutType::Card => Self::Card,
            api_models::enums::PayoutType::Wallet => Self::Wallet,
        }
    }
}

#[cfg(feature = "v1")]
impl ForeignTryFrom<&HeaderMap> for hyperswitch_domain_models::payments::HeaderPayload {
    type Error = error_stack::Report<errors::ApiErrorResponse>;
    fn foreign_try_from(headers: &HeaderMap) -> Result<Self, Self::Error> {
        let payment_confirm_source: Option<api_enums::PaymentSource> =
            get_header_value_by_key(X_PAYMENT_CONFIRM_SOURCE.into(), headers)?
                .map(|source| {
                    source
                        .to_owned()
                        .parse_enum("PaymentSource")
                        .change_context(errors::ApiErrorResponse::InvalidRequestData {
                            message: "Invalid data received in payment_confirm_source header"
                                .into(),
                        })
                        .attach_printable(
                            "Failed while paring PaymentConfirmSource header value to enum",
                        )
                })
                .transpose()?;
        when(
            payment_confirm_source.is_some_and(|payment_confirm_source| {
                payment_confirm_source.is_for_internal_use_only()
            }),
            || {
                Err(report!(errors::ApiErrorResponse::InvalidRequestData {
                    message: "Invalid data received in payment_confirm_source header".into(),
                }))
            },
        )?;
        let locale =
            get_header_value_by_key(ACCEPT_LANGUAGE.into(), headers)?.map(|val| val.to_string());
        let x_hs_latency = get_header_value_by_key(X_HS_LATENCY.into(), headers)
            .map(|value| value == Some("true"))
            .unwrap_or(false);

        let client_source =
            get_header_value_by_key(X_CLIENT_SOURCE.into(), headers)?.map(|val| val.to_string());

        let client_version =
            get_header_value_by_key(X_CLIENT_VERSION.into(), headers)?.map(|val| val.to_string());

        let browser_name_str =
            get_header_value_by_key(BROWSER_NAME.into(), headers)?.map(|val| val.to_string());

        let browser_name: Option<api_enums::BrowserName> = browser_name_str.map(|browser_name| {
            browser_name
                .parse_enum("BrowserName")
                .unwrap_or(api_enums::BrowserName::Unknown)
        });

        let x_client_platform_str =
            get_header_value_by_key(X_CLIENT_PLATFORM.into(), headers)?.map(|val| val.to_string());

        let x_client_platform: Option<api_enums::ClientPlatform> =
            x_client_platform_str.map(|x_client_platform| {
                x_client_platform
                    .parse_enum("ClientPlatform")
                    .unwrap_or(api_enums::ClientPlatform::Unknown)
            });

        let x_merchant_domain =
            get_header_value_by_key(X_MERCHANT_DOMAIN.into(), headers)?.map(|val| val.to_string());

        let x_app_id =
            get_header_value_by_key(X_APP_ID.into(), headers)?.map(|val| val.to_string());

        let x_redirect_uri =
            get_header_value_by_key(X_REDIRECT_URI.into(), headers)?.map(|val| val.to_string());

        Ok(Self {
            payment_confirm_source,
            client_source,
            client_version,
            x_hs_latency: Some(x_hs_latency),
            browser_name,
            x_client_platform,
            x_merchant_domain,
            locale,
            x_app_id,
            x_redirect_uri,
        })
    }
}

#[cfg(feature = "v2")]
impl ForeignTryFrom<&HeaderMap> for hyperswitch_domain_models::payments::HeaderPayload {
    type Error = error_stack::Report<errors::ApiErrorResponse>;
    fn foreign_try_from(headers: &HeaderMap) -> Result<Self, Self::Error> {
        use std::str::FromStr;

        use crate::headers::X_CLIENT_SECRET;

        let payment_confirm_source: Option<api_enums::PaymentSource> =
            get_header_value_by_key(X_PAYMENT_CONFIRM_SOURCE.into(), headers)?
                .map(|source| {
                    source
                        .to_owned()
                        .parse_enum("PaymentSource")
                        .change_context(errors::ApiErrorResponse::InvalidRequestData {
                            message: "Invalid data received in payment_confirm_source header"
                                .into(),
                        })
                        .attach_printable(
                            "Failed while paring PaymentConfirmSource header value to enum",
                        )
                })
                .transpose()?;
        when(
            payment_confirm_source.is_some_and(|payment_confirm_source| {
                payment_confirm_source.is_for_internal_use_only()
            }),
            || {
                Err(report!(errors::ApiErrorResponse::InvalidRequestData {
                    message: "Invalid data received in payment_confirm_source header".into(),
                }))
            },
        )?;
        let locale =
            get_header_value_by_key(ACCEPT_LANGUAGE.into(), headers)?.map(|val| val.to_string());
        let x_hs_latency = get_header_value_by_key(X_HS_LATENCY.into(), headers)
            .map(|value| value == Some("true"))
            .unwrap_or(false);

        let client_source =
            get_header_value_by_key(X_CLIENT_SOURCE.into(), headers)?.map(|val| val.to_string());

        let client_version =
            get_header_value_by_key(X_CLIENT_VERSION.into(), headers)?.map(|val| val.to_string());

        let browser_name_str =
            get_header_value_by_key(BROWSER_NAME.into(), headers)?.map(|val| val.to_string());

        let browser_name: Option<api_enums::BrowserName> = browser_name_str.map(|browser_name| {
            browser_name
                .parse_enum("BrowserName")
                .unwrap_or(api_enums::BrowserName::Unknown)
        });

        let x_client_platform_str =
            get_header_value_by_key(X_CLIENT_PLATFORM.into(), headers)?.map(|val| val.to_string());

        let x_client_platform: Option<api_enums::ClientPlatform> =
            x_client_platform_str.map(|x_client_platform| {
                x_client_platform
                    .parse_enum("ClientPlatform")
                    .unwrap_or(api_enums::ClientPlatform::Unknown)
            });

        let x_merchant_domain =
            get_header_value_by_key(X_MERCHANT_DOMAIN.into(), headers)?.map(|val| val.to_string());

        let x_app_id =
            get_header_value_by_key(X_APP_ID.into(), headers)?.map(|val| val.to_string());

        let x_redirect_uri =
            get_header_value_by_key(X_REDIRECT_URI.into(), headers)?.map(|val| val.to_string());

        Ok(Self {
            payment_confirm_source,
            // client_source,
            // client_version,
            x_hs_latency: Some(x_hs_latency),
            browser_name,
            x_client_platform,
            x_merchant_domain,
            locale,
            x_app_id,
            x_redirect_uri,
        })
    }
}

#[cfg(feature = "v1")]
impl
    ForeignTryFrom<(
        Option<&storage::PaymentAttempt>,
        Option<&storage::PaymentIntent>,
        Option<&domain::Address>,
        Option<&domain::Address>,
        Option<&domain::Customer>,
    )> for payments::PaymentsRequest
{
    type Error = error_stack::Report<errors::ApiErrorResponse>;
    fn foreign_try_from(
        value: (
            Option<&storage::PaymentAttempt>,
            Option<&storage::PaymentIntent>,
            Option<&domain::Address>,
            Option<&domain::Address>,
            Option<&domain::Customer>,
        ),
    ) -> Result<Self, Self::Error> {
        let (payment_attempt, payment_intent, shipping, billing, customer) = value;
        // Populating the dynamic fields directly, for the cases where we have customer details stored in
        // Payment Intent
        let customer_details_from_pi = payment_intent
            .and_then(|payment_intent| payment_intent.customer_details.clone())
            .map(|customer_details| {
                customer_details
                    .into_inner()
                    .peek()
                    .clone()
                    .parse_value::<CustomerData>("CustomerData")
                    .change_context(errors::ApiErrorResponse::InvalidDataValue {
                        field_name: "customer_details",
                    })
                    .attach_printable("Failed to parse customer_details")
            })
            .transpose()
            .change_context(errors::ApiErrorResponse::InvalidDataValue {
                field_name: "customer_details",
            })?;

        let mut billing_address = billing
            .map(hyperswitch_domain_models::address::Address::from)
            .map(api_types::Address::from);

        // This change is to fix a merchant integration
        // If billing.email is not passed by the merchant, and if the customer email is present, then use the `customer.email` as the billing email
        if let Some(billing_address) = &mut billing_address {
            billing_address.email = billing_address.email.clone().or_else(|| {
                customer
                    .and_then(|cust| {
                        cust.email
                            .as_ref()
                            .map(|email| pii::Email::from(email.clone()))
                    })
                    .or(customer_details_from_pi.clone().and_then(|cd| cd.email))
            });
        } else {
            billing_address = Some(payments::Address {
                email: customer
                    .and_then(|cust| {
                        cust.email
                            .as_ref()
                            .map(|email| pii::Email::from(email.clone()))
                    })
                    .or(customer_details_from_pi.clone().and_then(|cd| cd.email)),
                ..Default::default()
            });
        }

        Ok(Self {
            currency: payment_attempt.map(|pa| pa.currency.unwrap_or_default()),
            shipping: shipping
                .map(hyperswitch_domain_models::address::Address::from)
                .map(api_types::Address::from),
            billing: billing_address,
            amount: payment_attempt
                .map(|pa| api_types::Amount::from(pa.net_amount.get_order_amount())),
            email: customer
                .and_then(|cust| cust.email.as_ref().map(|em| pii::Email::from(em.clone())))
                .or(customer_details_from_pi.clone().and_then(|cd| cd.email)),
            phone: customer
                .and_then(|cust| cust.phone.as_ref().map(|p| p.clone().into_inner()))
                .or(customer_details_from_pi.clone().and_then(|cd| cd.phone)),
            name: customer
                .and_then(|cust| cust.name.as_ref().map(|n| n.clone().into_inner()))
                .or(customer_details_from_pi.clone().and_then(|cd| cd.name)),
            ..Self::default()
        })
    }
}

impl ForeignFrom<(storage::PaymentLink, payments::PaymentLinkStatus)>
    for payments::RetrievePaymentLinkResponse
{
    fn foreign_from(
        (payment_link_config, status): (storage::PaymentLink, payments::PaymentLinkStatus),
    ) -> Self {
        Self {
            payment_link_id: payment_link_config.payment_link_id,
            merchant_id: payment_link_config.merchant_id,
            link_to_pay: payment_link_config.link_to_pay,
            amount: payment_link_config.amount,
            created_at: payment_link_config.created_at,
            expiry: payment_link_config.fulfilment_time,
            description: payment_link_config.description,
            currency: payment_link_config.currency,
            status,
            secure_link: payment_link_config.secure_link,
        }
    }
}

impl From<domain::Address> for payments::AddressDetails {
    fn from(addr: domain::Address) -> Self {
        Self {
            city: addr.city,
            country: addr.country,
            line1: addr.line1.map(Encryptable::into_inner),
            line2: addr.line2.map(Encryptable::into_inner),
            line3: addr.line3.map(Encryptable::into_inner),
            zip: addr.zip.map(Encryptable::into_inner),
            state: addr.state.map(Encryptable::into_inner),
            first_name: addr.first_name.map(Encryptable::into_inner),
            last_name: addr.last_name.map(Encryptable::into_inner),
        }
    }
}

impl ForeignFrom<ConnectorSelection> for routing_types::StaticRoutingAlgorithm {
    fn foreign_from(value: ConnectorSelection) -> Self {
        match value {
            ConnectorSelection::Priority(connectors) => Self::Priority(connectors),

            ConnectorSelection::VolumeSplit(splits) => Self::VolumeSplit(splits),
        }
    }
}

impl ForeignFrom<api_models::organization::OrganizationNew>
    for diesel_models::organization::OrganizationNew
{
    fn foreign_from(item: api_models::organization::OrganizationNew) -> Self {
        Self::new(item.org_id, item.org_type, item.org_name)
    }
}

impl ForeignFrom<api_models::organization::OrganizationCreateRequest>
    for diesel_models::organization::OrganizationNew
{
    fn foreign_from(item: api_models::organization::OrganizationCreateRequest) -> Self {
        // Create a new organization with a standard type by default
        let org_new = api_models::organization::OrganizationNew::new(
            common_enums::OrganizationType::Standard,
            None,
        );
        let api_models::organization::OrganizationCreateRequest {
            organization_name,
            organization_details,
            metadata,
        } = item;
        let mut org_new_db = Self::new(org_new.org_id, org_new.org_type, Some(organization_name));
        org_new_db.organization_details = organization_details;
        org_new_db.metadata = metadata;
        org_new_db
    }
}

impl ForeignFrom<gsm_api_types::GsmCreateRequest> for storage::GatewayStatusMappingNew {
    fn foreign_from(value: gsm_api_types::GsmCreateRequest) -> Self {
        Self {
            connector: value.connector.to_string(),
            flow: value.flow,
            sub_flow: value.sub_flow,
            code: value.code,
            message: value.message,
            decision: value.decision.to_string(),
            status: value.status,
            router_error: value.router_error,
            step_up_possible: value.step_up_possible,
            unified_code: value.unified_code,
            unified_message: value.unified_message,
            error_category: value.error_category,
            clear_pan_possible: value.clear_pan_possible,
        }
    }
}

impl ForeignFrom<storage::GatewayStatusMap> for gsm_api_types::GsmResponse {
    fn foreign_from(value: storage::GatewayStatusMap) -> Self {
        Self {
            connector: value.connector.to_string(),
            flow: value.flow,
            sub_flow: value.sub_flow,
            code: value.code,
            message: value.message,
            decision: value.decision.to_string(),
            status: value.status,
            router_error: value.router_error,
            step_up_possible: value.step_up_possible,
            unified_code: value.unified_code,
            unified_message: value.unified_message,
            error_category: value.error_category,
            clear_pan_possible: value.clear_pan_possible,
        }
    }
}

#[cfg(all(feature = "v2", feature = "customer_v2"))]
impl ForeignFrom<&domain::Customer> for payments::CustomerDetailsResponse {
    fn foreign_from(_customer: &domain::Customer) -> Self {
        todo!()
    }
}

#[cfg(all(any(feature = "v1", feature = "v2"), not(feature = "customer_v2")))]
impl ForeignFrom<&domain::Customer> for payments::CustomerDetailsResponse {
    fn foreign_from(customer: &domain::Customer) -> Self {
        Self {
            id: Some(customer.customer_id.clone()),
            name: customer
                .name
                .as_ref()
                .map(|name| name.get_inner().to_owned()),
            email: customer.email.clone().map(Into::into),
            phone: customer
                .phone
                .as_ref()
                .map(|phone| phone.get_inner().to_owned()),
            phone_country_code: customer.phone_country_code.clone(),
        }
    }
}

#[cfg(feature = "olap")]
impl ForeignTryFrom<api_types::webhook_events::EventListConstraints>
    for api_types::webhook_events::EventListConstraintsInternal
{
    type Error = error_stack::Report<errors::ApiErrorResponse>;

    fn foreign_try_from(
        item: api_types::webhook_events::EventListConstraints,
    ) -> Result<Self, Self::Error> {
        if item.object_id.is_some()
            && (item.created_after.is_some()
                || item.created_before.is_some()
                || item.limit.is_some()
                || item.offset.is_some()
                || item.event_classes.is_some()
                || item.event_types.is_some())
        {
            return Err(report!(errors::ApiErrorResponse::PreconditionFailed {
                message:
                    "Either only `object_id` must be specified, or one or more of \
                          `created_after`, `created_before`, `limit`, `offset`, `event_classes` and `event_types` must be specified"
                        .to_string()
            }));
        }

        match item.object_id {
            Some(object_id) => Ok(Self::ObjectIdFilter { object_id }),
            None => Ok(Self::GenericFilter {
                created_after: item.created_after,
                created_before: item.created_before,
                limit: item.limit.map(i64::from),
                offset: item.offset.map(i64::from),
                event_classes: item.event_classes,
                event_types: item.event_types,
                is_delivered: item.is_delivered,
            }),
        }
    }
}

#[cfg(feature = "olap")]
impl TryFrom<domain::Event> for api_models::webhook_events::EventListItemResponse {
    type Error = error_stack::Report<errors::ApiErrorResponse>;

    fn try_from(item: domain::Event) -> Result<Self, Self::Error> {
        use crate::utils::OptionExt;

        // We only allow retrieving events with merchant_id, business_profile_id
        // and initial_attempt_id populated.
        // We cannot retrieve events with only some of these fields populated.
        let merchant_id = item
            .merchant_id
            .get_required_value("merchant_id")
            .change_context(errors::ApiErrorResponse::InternalServerError)?;
        let profile_id = item
            .business_profile_id
            .get_required_value("business_profile_id")
            .change_context(errors::ApiErrorResponse::InternalServerError)?;
        let initial_attempt_id = item
            .initial_attempt_id
            .get_required_value("initial_attempt_id")
            .change_context(errors::ApiErrorResponse::InternalServerError)?;

        Ok(Self {
            event_id: item.event_id,
            merchant_id,
            profile_id,
            object_id: item.primary_object_id,
            event_type: item.event_type,
            event_class: item.event_class,
            is_delivery_successful: item.is_overall_delivery_successful,
            initial_attempt_id,
            created: item.created_at,
        })
    }
}

#[cfg(feature = "olap")]
impl TryFrom<domain::Event> for api_models::webhook_events::EventRetrieveResponse {
    type Error = error_stack::Report<errors::ApiErrorResponse>;

    fn try_from(item: domain::Event) -> Result<Self, Self::Error> {
        use crate::utils::OptionExt;

        // We only allow retrieving events with all required fields in `EventListItemResponse`, and
        // `request` and `response` populated.
        // We cannot retrieve events with only some of these fields populated.
        let event_information =
            api_models::webhook_events::EventListItemResponse::try_from(item.clone())?;

        let request = item
            .request
            .get_required_value("request")
            .change_context(errors::ApiErrorResponse::InternalServerError)?
            .peek()
            .parse_struct("OutgoingWebhookRequestContent")
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Failed to parse webhook event request information")?;
        let response = item
            .response
            .get_required_value("response")
            .change_context(errors::ApiErrorResponse::InternalServerError)?
            .peek()
            .parse_struct("OutgoingWebhookResponseContent")
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Failed to parse webhook event response information")?;

        Ok(Self {
            event_information,
            request,
            response,
            delivery_attempt: item.delivery_attempt,
        })
    }
}

impl ForeignFrom<api_models::admin::AuthenticationConnectorDetails>
    for diesel_models::business_profile::AuthenticationConnectorDetails
{
    fn foreign_from(item: api_models::admin::AuthenticationConnectorDetails) -> Self {
        Self {
            authentication_connectors: item.authentication_connectors,
            three_ds_requestor_url: item.three_ds_requestor_url,
            three_ds_requestor_app_url: item.three_ds_requestor_app_url,
        }
    }
}

impl ForeignFrom<diesel_models::business_profile::AuthenticationConnectorDetails>
    for api_models::admin::AuthenticationConnectorDetails
{
    fn foreign_from(item: diesel_models::business_profile::AuthenticationConnectorDetails) -> Self {
        Self {
            authentication_connectors: item.authentication_connectors,
            three_ds_requestor_url: item.three_ds_requestor_url,
            three_ds_requestor_app_url: item.three_ds_requestor_app_url,
        }
    }
}

impl ForeignFrom<api_models::admin::ExternalVaultConnectorDetails>
    for diesel_models::business_profile::ExternalVaultConnectorDetails
{
    fn foreign_from(item: api_models::admin::ExternalVaultConnectorDetails) -> Self {
        Self {
            vault_connector_id: item.vault_connector_id,
            vault_sdk: item.vault_sdk,
        }
    }
}

impl ForeignFrom<diesel_models::business_profile::ExternalVaultConnectorDetails>
    for api_models::admin::ExternalVaultConnectorDetails
{
    fn foreign_from(item: diesel_models::business_profile::ExternalVaultConnectorDetails) -> Self {
        Self {
            vault_connector_id: item.vault_connector_id,
            vault_sdk: item.vault_sdk,
        }
    }
}

impl ForeignFrom<api_models::admin::CardTestingGuardConfig>
    for diesel_models::business_profile::CardTestingGuardConfig
{
    fn foreign_from(item: api_models::admin::CardTestingGuardConfig) -> Self {
        Self {
            is_card_ip_blocking_enabled: match item.card_ip_blocking_status {
                api_models::admin::CardTestingGuardStatus::Enabled => true,
                api_models::admin::CardTestingGuardStatus::Disabled => false,
            },
            card_ip_blocking_threshold: item.card_ip_blocking_threshold,
            is_guest_user_card_blocking_enabled: match item.guest_user_card_blocking_status {
                api_models::admin::CardTestingGuardStatus::Enabled => true,
                api_models::admin::CardTestingGuardStatus::Disabled => false,
            },
            guest_user_card_blocking_threshold: item.guest_user_card_blocking_threshold,
            is_customer_id_blocking_enabled: match item.customer_id_blocking_status {
                api_models::admin::CardTestingGuardStatus::Enabled => true,
                api_models::admin::CardTestingGuardStatus::Disabled => false,
            },
            customer_id_blocking_threshold: item.customer_id_blocking_threshold,
            card_testing_guard_expiry: item.card_testing_guard_expiry,
        }
    }
}

impl ForeignFrom<diesel_models::business_profile::CardTestingGuardConfig>
    for api_models::admin::CardTestingGuardConfig
{
    fn foreign_from(item: diesel_models::business_profile::CardTestingGuardConfig) -> Self {
        Self {
            card_ip_blocking_status: match item.is_card_ip_blocking_enabled {
                true => api_models::admin::CardTestingGuardStatus::Enabled,
                false => api_models::admin::CardTestingGuardStatus::Disabled,
            },
            card_ip_blocking_threshold: item.card_ip_blocking_threshold,
            guest_user_card_blocking_status: match item.is_guest_user_card_blocking_enabled {
                true => api_models::admin::CardTestingGuardStatus::Enabled,
                false => api_models::admin::CardTestingGuardStatus::Disabled,
            },
            guest_user_card_blocking_threshold: item.guest_user_card_blocking_threshold,
            customer_id_blocking_status: match item.is_customer_id_blocking_enabled {
                true => api_models::admin::CardTestingGuardStatus::Enabled,
                false => api_models::admin::CardTestingGuardStatus::Disabled,
            },
            customer_id_blocking_threshold: item.customer_id_blocking_threshold,
            card_testing_guard_expiry: item.card_testing_guard_expiry,
        }
    }
}

impl ForeignFrom<api_models::admin::WebhookDetails>
    for diesel_models::business_profile::WebhookDetails
{
    fn foreign_from(item: api_models::admin::WebhookDetails) -> Self {
        Self {
            webhook_version: item.webhook_version,
            webhook_username: item.webhook_username,
            webhook_password: item.webhook_password,
            webhook_url: item.webhook_url,
            payment_created_enabled: item.payment_created_enabled,
            payment_succeeded_enabled: item.payment_succeeded_enabled,
            payment_failed_enabled: item.payment_failed_enabled,
        }
    }
}

impl ForeignFrom<diesel_models::business_profile::WebhookDetails>
    for api_models::admin::WebhookDetails
{
    fn foreign_from(item: diesel_models::business_profile::WebhookDetails) -> Self {
        Self {
            webhook_version: item.webhook_version,
            webhook_username: item.webhook_username,
            webhook_password: item.webhook_password,
            webhook_url: item.webhook_url,
            payment_created_enabled: item.payment_created_enabled,
            payment_succeeded_enabled: item.payment_succeeded_enabled,
            payment_failed_enabled: item.payment_failed_enabled,
        }
    }
}

impl ForeignFrom<api_models::admin::BusinessPaymentLinkConfig>
    for diesel_models::business_profile::BusinessPaymentLinkConfig
{
    fn foreign_from(item: api_models::admin::BusinessPaymentLinkConfig) -> Self {
        Self {
            domain_name: item.domain_name,
            default_config: item.default_config.map(ForeignInto::foreign_into),
            business_specific_configs: item.business_specific_configs.map(|map| {
                map.into_iter()
                    .map(|(k, v)| (k, v.foreign_into()))
                    .collect()
            }),
            allowed_domains: item.allowed_domains,
            branding_visibility: item.branding_visibility,
        }
    }
}

impl ForeignFrom<diesel_models::business_profile::BusinessPaymentLinkConfig>
    for api_models::admin::BusinessPaymentLinkConfig
{
    fn foreign_from(item: diesel_models::business_profile::BusinessPaymentLinkConfig) -> Self {
        Self {
            domain_name: item.domain_name,
            default_config: item.default_config.map(ForeignInto::foreign_into),
            business_specific_configs: item.business_specific_configs.map(|map| {
                map.into_iter()
                    .map(|(k, v)| (k, v.foreign_into()))
                    .collect()
            }),
            allowed_domains: item.allowed_domains,
            branding_visibility: item.branding_visibility,
        }
    }
}

impl ForeignFrom<api_models::admin::PaymentLinkConfigRequest>
    for diesel_models::business_profile::PaymentLinkConfigRequest
{
    fn foreign_from(item: api_models::admin::PaymentLinkConfigRequest) -> Self {
        Self {
            theme: item.theme,
            logo: item.logo,
            seller_name: item.seller_name,
            sdk_layout: item.sdk_layout,
            display_sdk_only: item.display_sdk_only,
            enabled_saved_payment_method: item.enabled_saved_payment_method,
            hide_card_nickname_field: item.hide_card_nickname_field,
            show_card_form_by_default: item.show_card_form_by_default,
            details_layout: item.details_layout,
            background_image: item
                .background_image
                .map(|background_image| background_image.foreign_into()),
            payment_button_text: item.payment_button_text,
            skip_status_screen: item.skip_status_screen,
            custom_message_for_card_terms: item.custom_message_for_card_terms,
            payment_button_colour: item.payment_button_colour,
            background_colour: item.background_colour,
            payment_button_text_colour: item.payment_button_text_colour,
            sdk_ui_rules: item.sdk_ui_rules,
            payment_link_ui_rules: item.payment_link_ui_rules,
            enable_button_only_on_form_ready: item.enable_button_only_on_form_ready,
            payment_form_header_text: item.payment_form_header_text,
            payment_form_label_type: item.payment_form_label_type,
            show_card_terms: item.show_card_terms,
            is_setup_mandate_flow: item.is_setup_mandate_flow,
        }
    }
}

impl ForeignFrom<diesel_models::business_profile::PaymentLinkConfigRequest>
    for api_models::admin::PaymentLinkConfigRequest
{
    fn foreign_from(item: diesel_models::business_profile::PaymentLinkConfigRequest) -> Self {
        Self {
            theme: item.theme,
            logo: item.logo,
            seller_name: item.seller_name,
            sdk_layout: item.sdk_layout,
            display_sdk_only: item.display_sdk_only,
            enabled_saved_payment_method: item.enabled_saved_payment_method,
            hide_card_nickname_field: item.hide_card_nickname_field,
            show_card_form_by_default: item.show_card_form_by_default,
            transaction_details: None,
            details_layout: item.details_layout,
            background_image: item
                .background_image
                .map(|background_image| background_image.foreign_into()),
            payment_button_text: item.payment_button_text,
            skip_status_screen: item.skip_status_screen,
            custom_message_for_card_terms: item.custom_message_for_card_terms,
            payment_button_colour: item.payment_button_colour,
            background_colour: item.background_colour,
            payment_button_text_colour: item.payment_button_text_colour,
            sdk_ui_rules: item.sdk_ui_rules,
            payment_link_ui_rules: item.payment_link_ui_rules,
            enable_button_only_on_form_ready: item.enable_button_only_on_form_ready,
            payment_form_header_text: item.payment_form_header_text,
            payment_form_label_type: item.payment_form_label_type,
            show_card_terms: item.show_card_terms,
            is_setup_mandate_flow: item.is_setup_mandate_flow,
        }
    }
}

impl ForeignFrom<diesel_models::business_profile::PaymentLinkBackgroundImageConfig>
    for api_models::admin::PaymentLinkBackgroundImageConfig
{
    fn foreign_from(
        item: diesel_models::business_profile::PaymentLinkBackgroundImageConfig,
    ) -> Self {
        Self {
            url: item.url,
            position: item.position,
            size: item.size,
        }
    }
}

impl ForeignFrom<api_models::admin::PaymentLinkBackgroundImageConfig>
    for diesel_models::business_profile::PaymentLinkBackgroundImageConfig
{
    fn foreign_from(item: api_models::admin::PaymentLinkBackgroundImageConfig) -> Self {
        Self {
            url: item.url,
            position: item.position,
            size: item.size,
        }
    }
}

impl ForeignFrom<api_models::admin::BusinessPayoutLinkConfig>
    for diesel_models::business_profile::BusinessPayoutLinkConfig
{
    fn foreign_from(item: api_models::admin::BusinessPayoutLinkConfig) -> Self {
        Self {
            config: item.config.foreign_into(),
            form_layout: item.form_layout,
            payout_test_mode: item.payout_test_mode,
        }
    }
}

impl ForeignFrom<diesel_models::business_profile::BusinessPayoutLinkConfig>
    for api_models::admin::BusinessPayoutLinkConfig
{
    fn foreign_from(item: diesel_models::business_profile::BusinessPayoutLinkConfig) -> Self {
        Self {
            config: item.config.foreign_into(),
            form_layout: item.form_layout,
            payout_test_mode: item.payout_test_mode,
        }
    }
}

impl ForeignFrom<api_models::admin::BusinessGenericLinkConfig>
    for diesel_models::business_profile::BusinessGenericLinkConfig
{
    fn foreign_from(item: api_models::admin::BusinessGenericLinkConfig) -> Self {
        Self {
            domain_name: item.domain_name,
            allowed_domains: item.allowed_domains,
            ui_config: item.ui_config,
        }
    }
}

impl ForeignFrom<diesel_models::business_profile::BusinessGenericLinkConfig>
    for api_models::admin::BusinessGenericLinkConfig
{
    fn foreign_from(item: diesel_models::business_profile::BusinessGenericLinkConfig) -> Self {
        Self {
            domain_name: item.domain_name,
            allowed_domains: item.allowed_domains,
            ui_config: item.ui_config,
        }
    }
}

impl ForeignFrom<card_info_types::CardInfoCreateRequest> for storage::CardInfo {
    fn foreign_from(value: card_info_types::CardInfoCreateRequest) -> Self {
        Self {
            card_iin: value.card_iin,
            card_issuer: value.card_issuer,
            card_network: value.card_network,
            card_type: value.card_type,
            card_subtype: value.card_subtype,
            card_issuing_country: value.card_issuing_country,
            bank_code_id: value.bank_code_id,
            bank_code: value.bank_code,
            country_code: value.country_code,
            date_created: common_utils::date_time::now(),
            last_updated: Some(common_utils::date_time::now()),
            last_updated_provider: value.last_updated_provider,
        }
    }
}

impl ForeignFrom<card_info_types::CardInfoUpdateRequest> for storage::CardInfo {
    fn foreign_from(value: card_info_types::CardInfoUpdateRequest) -> Self {
        Self {
            card_iin: value.card_iin,
            card_issuer: value.card_issuer,
            card_network: value.card_network,
            card_type: value.card_type,
            card_subtype: value.card_subtype,
            card_issuing_country: value.card_issuing_country,
            bank_code_id: value.bank_code_id,
            bank_code: value.bank_code,
            country_code: value.country_code,
            date_created: common_utils::date_time::now(),
            last_updated: Some(common_utils::date_time::now()),
            last_updated_provider: value.last_updated_provider,
        }
    }
}
