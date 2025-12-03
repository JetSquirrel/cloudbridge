# Contributing to CloudBridge

First off, thank you for considering contributing to CloudBridge! It's people like you that make CloudBridge such a great tool.

## Code of Conduct

By participating in this project, you are expected to uphold our Code of Conduct: be respectful, inclusive, and constructive in all interactions.

## How Can I Contribute?

### Reporting Bugs

Before creating bug reports, please check the existing issues as you might find out that you don't need to create one. When you are creating a bug report, please include as many details as possible:

- **Use a clear and descriptive title**
- **Describe the exact steps which reproduce the problem**
- **Provide specific examples to demonstrate the steps**
- **Describe the behavior you observed after following the steps**
- **Explain which behavior you expected to see instead and why**
- **Include screenshots if possible**
- **Include your environment details** (OS, Rust version, etc.)

### Suggesting Enhancements

Enhancement suggestions are tracked as GitHub issues. When creating an enhancement suggestion, please include:

- **Use a clear and descriptive title**
- **Provide a step-by-step description of the suggested enhancement**
- **Provide specific examples to demonstrate the steps**
- **Describe the current behavior and explain which behavior you expected**
- **Explain why this enhancement would be useful**

### Pull Requests

1. Fork the repo and create your branch from `main`
2. If you've added code that should be tested, add tests
3. If you've changed APIs, update the documentation
4. Ensure the code compiles without warnings
5. Make sure your code follows the existing style
6. Issue that pull request!

## Development Setup

### Prerequisites

- Rust 1.75 or later
- Windows SDK (for Windows builds)

### Building

```bash
# Clone your fork
git clone https://github.com/YOUR_USERNAME/cloudbridge.git
cd cloudbridge

# Set up pre-commit hooks (recommended)
git config core.hooksPath .githooks

# Build
cargo build

# Run tests
cargo test

# Check for warnings
cargo clippy
```

### Code Style

- Follow Rust's official style guidelines
- Run `cargo fmt` before committing (enforced by pre-commit hook)
- Run `cargo clippy` to catch common mistakes (enforced by pre-commit hook)
- Write documentation for public APIs
- Use meaningful variable and function names

### Commit Messages

- Use the present tense ("Add feature" not "Added feature")
- Use the imperative mood ("Move cursor to..." not "Moves cursor to...")
- Limit the first line to 72 characters or less
- Reference issues and pull requests liberally after the first line

### Project Structure

```
cloudbridge/
├── src/
│   ├── main.rs          # Application entry point
│   ├── app.rs           # Main application logic
│   ├── config.rs        # Configuration management
│   ├── crypto.rs        # Encryption utilities
│   ├── db.rs            # Database operations
│   ├── cloud/           # Cloud provider implementations
│   │   ├── mod.rs       # Cloud traits and types
│   │   ├── aws.rs       # AWS implementation
│   │   └── aliyun.rs    # Alibaba Cloud implementation
│   └── ui/              # User interface components
│       ├── mod.rs       # UI module exports
│       ├── dashboard.rs # Dashboard view
│       ├── accounts.rs  # Account management view
│       ├── settings.rs  # Settings view
│       └── chart.rs     # Chart components
├── Cargo.toml           # Dependencies
└── README.md            # Documentation
```

## Adding a New Cloud Provider

To add support for a new cloud provider:

1. Create a new file in `src/cloud/` (e.g., `azure.rs`)
2. Implement the `CloudService` trait
3. Add the provider to `CloudProvider` enum in `src/cloud/mod.rs`
4. Update the UI in `src/ui/accounts.rs` to support the new provider
5. Add documentation in README.md
6. Test thoroughly with real credentials

### CloudService Trait

```rust
pub trait CloudService {
    /// Validate that the credentials are correct
    fn validate_credentials(&self) -> Result<bool>;
    
    /// Get cost summary for the account
    fn get_cost_summary(&self) -> Result<CostSummary>;
    
    /// Get daily cost trend data
    fn get_cost_trend(&self) -> Result<CostTrend>;
}
```

## Questions?

Feel free to open an issue with the tag "question" if you have any questions about contributing.

Thank you for your contribution! 🎉
