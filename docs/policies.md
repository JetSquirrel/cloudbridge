# Provider permissions and credential handling

Use dedicated credentials with the least privilege needed for the channel
you enable. Read-only application behavior does not make an unrestricted
key read-only: the provider's attached policies determine its permissions.
Do not use root-account credentials or grant administrator access just to
read a bill.

CloudBridge stores credentials in the OS keyring and uses them to
authenticate provider requests. It has no CloudBridge sync service or
telemetry. Billing databases and retained raw files are not encrypted by
the app; protect them with appropriate device, filesystem and backup
controls. Never post real keys, credential files or unredacted billing
exports in public issues, logs, screenshots or pull requests. Revoke and
replace any key that has been exposed.

## AWS Cost Explorer

### Minimal Cost Explorer policy

File: [`aws-cost-explorer-policy.json`](aws-cost-explorer-policy.json)

This policy grants only `ce:GetCostAndUsage`, the Cost Explorer action
used by `src/cloud/aws.rs` for billing retrieval. Credential validation
when no export URI is set uses STS `GetCallerIdentity`, which
[requires no permission grant](https://docs.aws.amazon.com/STS/latest/APIReference/API_GetCallerIdentity.html).
This template retains `Resource: "*"` for Cost Explorer reads; it does not
grant access to every AWS service. Review resource scoping against AWS's
current authorization model and your billing configuration.
Organization policies and account-level billing settings can still restrict
access. Cost Explorer requests can incur fees.

```json
{
    "Version": "2012-10-17",
    "Statement": [
        {
            "Effect": "Allow",
            "Action": [
                "ce:GetCostAndUsage"
            ],
            "Resource": "*"
        }
    ]
}
```

### Applying the policy

1. In AWS IAM, create a customer-managed policy from the template.
2. Attach it to a dedicated IAM user, without unrelated broad permissions.
3. Create an access key for that user and enter it in CloudBridge locally.
4. Review and rotate the key according to your organization's policy.

The current client accepts an access-key ID and secret-access-key pair;
it does not support temporary credentials that require a session token.

### AWS Data Exports from S3 — working tree / unreleased

The S3 export channel is listed under **Unreleased** in the
[changelog](../CHANGELOG.md); it is not part of 0.3.1. The Cost Explorer
policy above does **not** grant access to an export bucket.

For this channel, scope `s3:ListBucket` to the export bucket and permitted
prefix, and `s3:GetObject` to the objects under that prefix. If objects
use a customer-managed KMS key, decryption may also require
`kms:Decrypt` and a compatible key policy. Bucket policies and any
organization-level restrictions must allow the reads as well. CloudBridge
reads an existing export; it does not need permissions to create exports
or write objects. Export delivery permissions are a separate AWS setup.

S3 collection avoids Cost Explorer request charges, but S3 storage and
request fees still apply. Encryption, retrieval or transfer charges may
also apply depending on your AWS configuration.

## Alibaba Cloud

For the billing API channel, `AliyunBSSReadOnlyAccess` is the built-in
read-only billing policy. It is broader than an endpoint-specific custom
policy; review its scope against your requirements.

1. In the RAM console, create a dedicated RAM user.
2. Attach `AliyunBSSReadOnlyAccess`, or an appropriately scoped custom
   policy for the billing operations you use.
3. Create an AccessKey and enter it in CloudBridge locally.

Do not use the primary account's AccessKey. A local bill-file import does
not require an API credential, including for Alibaba Cloud.

## DeepSeek

CloudBridge's API integration uses a platform key from
[platform.deepseek.com](https://platform.deepseek.com/) to query
`GET /user/balance`. This is a read operation, but **the key itself is not
necessarily read-only**: it may also authorize model requests and incur
spend if used elsewhere. Treat it as a spending credential, use any
provider-supported restrictions, and do not share it.

The balance API does not provide spend detail. Import the console's cost
export for that information; import alone requires no API key. DeepSeek's
zip can be imported directly: CloudBridge selects the cost CSV, not the
token-count CSV whose `amount` column represents usage rather than money.

## Volcengine, OpenAI and Anthropic

These sources currently use local bill exports only. CloudBridge does not
ask for or store credentials for them, and file import makes no provider
request. Do not create an admin or organization key for these integrations
until an API channel actually requires it. Permissions for future API
support will be documented with that implementation.

See the [import guide](https://cloudbridge.jetsquirrel.cloud/docs.html#import)
for supported exports. Imports replace the entire month for the selected
account and source, not just matching rows; use a complete, unfiltered
export. Force Refresh preserves imported months. Usage-only exports are
not spend, and text exports must be UTF-8 (re-export or save as CSV UTF-8
if the original download uses another encoding).
