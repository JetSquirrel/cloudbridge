//! Each vendor's products, placed in FOCUS `ServiceCategory` and
//! `ServiceSubcategory`.
//!
//! The ledger's `service_category` holds only values the specification
//! allows, so a category means the same thing whichever cloud billed it —
//! "compute, across everything" is then one `GROUP BY`. A source that
//! reports FOCUS itself (AWS's Data Exports) passes its own values through;
//! everything else is placed here, at write time, from the vendor's product
//! code or, for AWS's Cost Explorer, which reports no code, its service
//! name.
//!
//! The tables are deliberately short. A product they do not know stays
//! uncategorized and is reported as a data-quality finding: a missing
//! category is visible and fixable, a guessed one silently moves money
//! between categories.
//!
//! Pure and target-independent: both backends call [`fill`] on the way in.

use crate::model::Charge;

/// FOCUS 1.2 `ServiceCategory` values.
pub const CATEGORIES: &[&str] = &[
    "AI and Machine Learning",
    "Analytics",
    "Business Applications",
    "Compute",
    "Databases",
    "Developer Tools",
    "Multicloud",
    "Identity",
    "Integration",
    "Internet of Things",
    "Management and Governance",
    "Media",
    "Migration",
    "Mobile",
    "Networking",
    "Security",
    "Storage",
    "Web",
    "Other",
];

const AI: &str = "AI and Machine Learning";
const COMPUTE: &str = "Compute";
const DATABASES: &str = "Databases";
const INTEGRATION: &str = "Integration";
const MANAGEMENT: &str = "Management and Governance";
const NETWORKING: &str = "Networking";
const SECURITY: &str = "Security";
const STORAGE: &str = "Storage";
const WEB: &str = "Web";

/// What a breakdown calls a charge no table placed. Not `"Other"`: that is a
/// FOCUS category of its own, and a vendor's export can use it.
pub const UNCATEGORIZED: &str = "Uncategorized";

/// A FOCUS `(ServiceCategory, ServiceSubcategory)` pair.
pub type Placement = (&'static str, &'static str);

/// The FOCUS 1.2 `ServiceSubcategory` values this module places products
/// in, each under its category. A test holds every table entry to this
/// list, so a typo cannot reach the ledger.
pub const SUBCATEGORIES: &[Placement] = &[
    (AI, "Generative AI"),
    (AI, "Machine Learning"),
    (COMPUTE, "Containers"),
    (COMPUTE, "Serverless Compute"),
    (COMPUTE, "Virtual Machines"),
    (COMPUTE, "Other (Compute)"),
    (DATABASES, "Caching"),
    (DATABASES, "NoSQL Databases"),
    (DATABASES, "Relational Databases"),
    (INTEGRATION, "API Management"),
    (MANAGEMENT, "Cost Management"),
    (MANAGEMENT, "Observability"),
    (NETWORKING, "Application Networking"),
    (NETWORKING, "Content Delivery"),
    (NETWORKING, "Network Connectivity"),
    (NETWORKING, "Network Infrastructure"),
    (NETWORKING, "Network Routing"),
    (NETWORKING, "Network Security"),
    (SECURITY, "Secret Management"),
    (SECURITY, "Threat Detection and Response"),
    (STORAGE, "Block Storage"),
    (STORAGE, "File Storage"),
    (STORAGE, "Object Storage"),
    (WEB, "Application Platforms"),
];

/// AWS, by `x_ServiceCode` — what a Data Export row and the demo bill carry.
const AWS_CODES: &[(&str, Placement)] = &[
    ("AmazonEC2", (COMPUTE, "Virtual Machines")),
    ("AWSLambda", (COMPUTE, "Serverless Compute")),
    ("AmazonECS", (COMPUTE, "Containers")),
    ("AmazonEKS", (COMPUTE, "Containers")),
    ("AmazonS3", (STORAGE, "Object Storage")),
    ("AmazonEFS", (STORAGE, "File Storage")),
    ("AmazonRDS", (DATABASES, "Relational Databases")),
    ("AmazonDynamoDB", (DATABASES, "NoSQL Databases")),
    ("AmazonElastiCache", (DATABASES, "Caching")),
    ("AmazonBedrock", (AI, "Generative AI")),
    ("AmazonSageMaker", (AI, "Machine Learning")),
    ("AmazonCloudWatch", (MANAGEMENT, "Observability")),
    ("AmazonVPC", (NETWORKING, "Network Infrastructure")),
    ("AmazonCloudFront", (NETWORKING, "Content Delivery")),
    ("AmazonRoute53", (NETWORKING, "Network Routing")),
    ("AWSELB", (NETWORKING, "Application Networking")),
    ("AWSDataTransfer", (NETWORKING, "Network Connectivity")),
    ("awskms", (SECURITY, "Secret Management")),
];

/// AWS, by the `SERVICE` names Cost Explorer groups by — it reports no code.
const AWS_CE_NAMES: &[(&str, Placement)] = &[
    (
        "Amazon Elastic Compute Cloud - Compute",
        (COMPUTE, "Virtual Machines"),
    ),
    ("EC2 - Other", (COMPUTE, "Other (Compute)")),
    ("AWS Lambda", (COMPUTE, "Serverless Compute")),
    ("Amazon Elastic Container Service", (COMPUTE, "Containers")),
    (
        "Amazon Elastic Container Service for Kubernetes",
        (COMPUTE, "Containers"),
    ),
    ("Amazon Simple Storage Service", (STORAGE, "Object Storage")),
    ("Amazon Elastic File System", (STORAGE, "File Storage")),
    (
        "Amazon Relational Database Service",
        (DATABASES, "Relational Databases"),
    ),
    ("Amazon DynamoDB", (DATABASES, "NoSQL Databases")),
    ("Amazon ElastiCache", (DATABASES, "Caching")),
    ("Amazon Bedrock", (AI, "Generative AI")),
    ("Amazon SageMaker", (AI, "Machine Learning")),
    ("AmazonCloudWatch", (MANAGEMENT, "Observability")),
    ("Amazon CloudWatch", (MANAGEMENT, "Observability")),
    (
        "Amazon Virtual Private Cloud",
        (NETWORKING, "Network Infrastructure"),
    ),
    ("Amazon CloudFront", (NETWORKING, "Content Delivery")),
    ("Amazon Route 53", (NETWORKING, "Network Routing")),
    (
        "Elastic Load Balancing",
        (NETWORKING, "Application Networking"),
    ),
    ("AWS Data Transfer", (NETWORKING, "Network Connectivity")),
    (
        "AWS Key Management Service",
        (SECURITY, "Secret Management"),
    ),
    ("AWS Secrets Manager", (SECURITY, "Secret Management")),
    ("AWS Cost Explorer", (MANAGEMENT, "Cost Management")),
    ("AWS AppSync", (INTEGRATION, "API Management")),
    ("AWS Amplify", (WEB, "Application Platforms")),
];

/// Alibaba Cloud, by product code (`ProductCode` / 产品代码).
const ALIYUN_CODES: &[(&str, Placement)] = &[
    ("ecs", (COMPUTE, "Virtual Machines")),
    ("fc", (COMPUTE, "Serverless Compute")),
    ("cs", (COMPUTE, "Containers")),
    ("oss", (STORAGE, "Object Storage")),
    ("nas", (STORAGE, "File Storage")),
    ("ebs", (STORAGE, "Block Storage")),
    ("rds", (DATABASES, "Relational Databases")),
    ("polardb", (DATABASES, "Relational Databases")),
    ("kvstore", (DATABASES, "Caching")),
    ("dds", (DATABASES, "NoSQL Databases")),
    ("bailian", (AI, "Generative AI")),
    ("pai", (AI, "Machine Learning")),
    ("sls", (MANAGEMENT, "Observability")),
    ("cms", (MANAGEMENT, "Observability")),
    ("cdn", (NETWORKING, "Content Delivery")),
    ("slb", (NETWORKING, "Application Networking")),
    ("vpc", (NETWORKING, "Network Infrastructure")),
    ("eip", (NETWORKING, "Network Connectivity")),
    ("cbn", (NETWORKING, "Network Connectivity")),
    ("alidns", (NETWORKING, "Network Routing")),
    ("waf", (NETWORKING, "Network Security")),
    ("kms", (SECURITY, "Secret Management")),
    ("sas", (SECURITY, "Threat Detection and Response")),
];

/// Volcengine, by product code (`Product`).
const VOLCENGINE_CODES: &[(&str, Placement)] = &[
    ("ecs", (COMPUTE, "Virtual Machines")),
    ("vke", (COMPUTE, "Containers")),
    ("tos", (STORAGE, "Object Storage")),
    ("rds_mysql", (DATABASES, "Relational Databases")),
    ("redis", (DATABASES, "Caching")),
    ("ark", (AI, "Generative AI")),
    ("cdn", (NETWORKING, "Content Delivery")),
    ("clb", (NETWORKING, "Application Networking")),
    ("vpc", (NETWORKING, "Network Infrastructure")),
    ("eip", (NETWORKING, "Network Connectivity")),
];

/// The sources whose whole bill is model inference.
const MODEL_PROVIDERS: &[&str] = &["OpenAI", "Anthropic", "DeepSeek"];

fn lookup(table: &[(&str, Placement)], key: &str) -> Option<Placement> {
    let key = key.trim();
    table
        .iter()
        .find(|(entry, _)| entry.eq_ignore_ascii_case(key))
        .map(|(_, placement)| *placement)
}

/// Where a provider's product belongs, from its product code or, failing
/// that, its service name. `None` when the tables do not know it.
pub fn classify(
    provider: &str,
    service_code: Option<&str>,
    service_name: Option<&str>,
) -> Option<Placement> {
    if MODEL_PROVIDERS.contains(&provider) {
        return Some((AI, "Generative AI"));
    }
    let code = service_code.filter(|code| !code.trim().is_empty());
    match provider {
        "AWS" => code
            .and_then(|code| lookup(AWS_CODES, code))
            .or_else(|| service_name.and_then(|name| lookup(AWS_CE_NAMES, name))),
        "Aliyun" => code.and_then(|code| lookup(ALIYUN_CODES, code)),
        "Volcengine" => code.and_then(|code| lookup(VOLCENGINE_CODES, code)),
        _ => None,
    }
}

/// Place a charge that arrived without a category. One a source already
/// set — AWS's own FOCUS values — is left as it is, subcategory included.
pub fn fill(provider: &str, charge: &mut Charge) {
    if charge.service_category.is_some() {
        return;
    }
    if let Some((category, subcategory)) = classify(
        provider,
        charge.x_service_code.as_deref(),
        charge.service_name.as_deref(),
    ) {
        charge.service_category = Some(category.to_string());
        charge.service_subcategory = Some(subcategory.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn every_entry() -> impl Iterator<Item = (&'static str, Placement)> {
        [AWS_CODES, AWS_CE_NAMES, ALIYUN_CODES, VOLCENGINE_CODES]
            .into_iter()
            .flatten()
            .copied()
    }

    #[test]
    fn every_placement_is_a_focus_category_and_subcategory_pair() {
        for (key, placement) in every_entry() {
            assert!(
                CATEGORIES.contains(&placement.0),
                "{key}: {:?} is not a FOCUS ServiceCategory",
                placement.0
            );
            assert!(
                SUBCATEGORIES.contains(&placement),
                "{key}: {placement:?} is not a listed subcategory of its category"
            );
        }
        for (category, _) in SUBCATEGORIES {
            assert!(CATEGORIES.contains(category));
        }
    }

    #[test]
    fn no_table_lists_a_key_twice() {
        for table in [AWS_CODES, AWS_CE_NAMES, ALIYUN_CODES, VOLCENGINE_CODES] {
            let mut keys: Vec<String> = table
                .iter()
                .map(|(key, _)| key.to_ascii_lowercase())
                .collect();
            keys.sort();
            let before = keys.len();
            keys.dedup();
            assert_eq!(before, keys.len());
        }
    }

    #[test]
    fn the_same_kind_of_product_lands_in_the_same_place_on_every_cloud() {
        let vm = Some((COMPUTE, "Virtual Machines"));
        assert_eq!(classify("AWS", Some("AmazonEC2"), None), vm);
        assert_eq!(classify("Aliyun", Some("ecs"), None), vm);
        assert_eq!(classify("Volcengine", Some("ecs"), None), vm);

        let objects = Some((STORAGE, "Object Storage"));
        assert_eq!(classify("AWS", Some("AmazonS3"), None), objects);
        assert_eq!(classify("Aliyun", Some("oss"), None), objects);
        assert_eq!(classify("Volcengine", Some("tos"), None), objects);

        let inference = Some((AI, "Generative AI"));
        assert_eq!(classify("Aliyun", Some("bailian"), None), inference);
        assert_eq!(classify("Volcengine", Some("ark"), None), inference);
        assert_eq!(classify("OpenAI", None, Some("OpenAI")), inference);
        assert_eq!(classify("DeepSeek", None, None), inference);
    }

    #[test]
    fn cost_explorer_rows_are_placed_by_service_name() {
        assert_eq!(
            classify("AWS", None, Some("Amazon Simple Storage Service")),
            Some((STORAGE, "Object Storage"))
        );
        // A code wins over the name when both are there.
        assert_eq!(
            classify(
                "AWS",
                Some("AWSLambda"),
                Some("Amazon Simple Storage Service")
            ),
            Some((COMPUTE, "Serverless Compute"))
        );
    }

    #[test]
    fn codes_match_without_regard_to_case_or_surrounding_space() {
        assert_eq!(
            classify("Volcengine", Some(" ARK "), None),
            Some((AI, "Generative AI"))
        );
    }

    #[test]
    fn an_unknown_product_stays_uncategorized() {
        assert_eq!(classify("Aliyun", Some("some-new-product"), None), None);
        assert_eq!(classify("Aliyun", None, Some("云服务器 ECS")), None);
        assert_eq!(classify("Aliyun", Some(""), None), None);
        assert_eq!(classify("SomeCloud", Some("ecs"), None), None);
    }

    #[test]
    fn fill_leaves_a_source_s_own_category_alone() {
        let now = Utc::now();
        let mut focus = Charge {
            service_category: Some("Compute".to_string()),
            x_service_code: Some("AmazonS3".to_string()),
            ..Charge::new(now, now, "USD")
        };
        fill("AWS", &mut focus);
        assert_eq!(focus.service_category.as_deref(), Some("Compute"));
        assert_eq!(focus.service_subcategory, None);

        let mut aliyun = Charge {
            x_service_code: Some("oss".to_string()),
            ..Charge::new(now, now, "CNY")
        };
        fill("Aliyun", &mut aliyun);
        assert_eq!(aliyun.service_category.as_deref(), Some(STORAGE));
        assert_eq!(
            aliyun.service_subcategory.as_deref(),
            Some("Object Storage")
        );
    }
}
