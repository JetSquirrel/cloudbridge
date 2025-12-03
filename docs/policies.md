# Policy Files

This directory contains IAM policy templates for different cloud providers.

## AWS

### Minimum Required Permissions

File: `aws-cost-explorer-policy.json`

This policy grants read-only access to AWS Cost Explorer API.

```json
{
    "Version": "2012-10-17",
    "Statement": [
        {
            "Effect": "Allow",
            "Action": [
                "ce:GetCostAndUsage",
                "ce:GetCostForecast",
                "ce:GetDimensionValues",
                "ce:GetTags"
            ],
            "Resource": "*"
        }
    ]
}
```

### How to Apply

1. Go to AWS IAM Console
2. Create a new policy with the JSON above
3. Create a new IAM user for programmatic access
4. Attach the policy to the user
5. Generate Access Keys

## Alibaba Cloud

### Required Permissions

Grant the built-in policy: `AliyunBSSReadOnlyAccess`

This policy provides read-only access to billing and cost management APIs.

### How to Apply

1. Go to Alibaba Cloud RAM Console
2. Create a new RAM user
3. Attach the `AliyunBSSReadOnlyAccess` system policy
4. Create an AccessKey for the user
