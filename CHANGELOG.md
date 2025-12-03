# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Initial release preparation
- Comprehensive documentation

## [0.1.0] - 2024-12-03

### Added
- **AWS Integration**
  - AWS Cost Explorer API integration with manual AWS Signature V4 signing
  - Current/previous month cost comparison
  - Per-service cost breakdown
  - 30-day cost trend visualization

- **Alibaba Cloud Integration**
  - Alibaba Cloud BSS API integration with HMAC-SHA1 signing
  - Bill overview and instance bill queries
  - Per-product cost breakdown
  - Monthly cost trend visualization

- **Dashboard**
  - Cost overview cards with month-over-month change
  - Account-level cost summaries
  - Expandable service-level details
  - Cost trend charts with statistics

- **Account Management**
  - Add/remove cloud accounts
  - Credential validation before saving
  - Support for AWS and Alibaba Cloud

- **Data Management**
  - DuckDB local storage
  - AES-256-GCM credential encryption
  - 6-hour intelligent caching
  - Force refresh capability

- **User Interface**
  - GPUI-based modern desktop UI
  - Dark theme
  - Responsive sidebar navigation
  - Settings panel

### Security
- All credentials encrypted at rest using AES-256-GCM
- No network transmission except direct cloud API calls
- Local-only data storage

### Known Issues
- Windows only (macOS/Linux support planned)
- Requires Windows SDK for building (fxc.exe shader compiler)

---

## Version History

- **0.1.0** - Initial release with AWS and Alibaba Cloud support
