# CloudBridge Claude Skills

This directory contains Claude Code skills to help with CloudBridge development. These skills provide guidance, patterns, and best practices specific to this GPUI Rust project.

## Available Skills

### 1. GPUI Style Guide (`gpui-style-guide/`)
Provides GPUI framework coding standards and best practices.

**Use when:**
- Writing new GPUI UI components
- Refactoring existing UI code
- Reviewing code for GPUI patterns
- Need guidance on GPUI best practices

**Key Topics:**
- Component structure patterns
- Styling order conventions
- Theme system usage
- State management
- Async operations
- Performance optimization

### 2. GPUI Testing (`gpui-test/`)
Testing patterns and best practices for GPUI applications.

**Use when:**
- Writing unit tests for GPUI components
- Testing async operations in UI
- Creating integration tests
- Need test examples

**Key Topics:**
- GPUI test setup
- State change testing
- Async operation testing
- Cloud provider testing
- Database operation testing
- Test organization

### 3. Refactor Large Files (`refactor-large-files/`)
Helps identify and refactor large files to improve maintainability.

**Use when:**
- A file exceeds 500 lines of code
- A component has multiple responsibilities
- Code review suggests splitting files
- Adding new features to already large files

**Key Topics:**
- Extract subcomponents pattern
- Separate data logic from UI
- State management extraction
- Module structure organization
- Step-by-step refactoring process

**Target Files:**
- `src/ui/dashboard.rs` (837 lines)
- `src/ui/accounts.rs` (684 lines)
- `src/cloud/aws.rs` (758 lines)
- `src/ui/chart.rs` (545 lines)

### 4. Reduce Clones (`reduce-clones/`)
Identifies and eliminates unnecessary `.clone()` calls to optimize memory usage and performance.

**Use when:**
- Performance profiling shows excessive memory allocation
- Code review identifies unnecessary clones
- Refactoring to improve efficiency
- Adding new features and want to avoid clone overhead

**Key Topics:**
- Common clone patterns
- Reference usage
- Arc for shared ownership
- String optimization
- Closure capture optimization
- Clone alternatives cheat sheet

### 5. Add Cloud Provider (`add-cloud-provider/`)
Guides you through adding a new cloud provider integration.

**Use when:**
- Adding support for a new cloud platform (Azure, GCP, etc.)
- Implementing a new cost API integration
- Extending CloudBridge with custom providers

**Key Topics:**
- CloudService trait implementation
- Authentication patterns (OAuth, Signature, API Key)
- API request/response handling
- UI integration
- Testing with mock APIs
- Provider-specific considerations

### 6. Security Audit (`security-audit/`)
Helps perform security audits and identify potential vulnerabilities.

**Use when:**
- Before releases to check for security issues
- Reviewing new code for vulnerabilities
- Implementing security-sensitive features
- Responding to security reports

**Key Topics:**
- Credential management review
- Encryption security audit
- Input validation checklist
- API security verification
- Dependency security checks
- OWASP Top 10 compliance

### 7. Debug GPUI Issues (`debug-gpui/`)
Helps debug common GPUI framework issues.

**Use when:**
- UI not rendering correctly
- State updates not reflecting in UI
- Performance issues or lag
- Layout problems
- Event handlers not firing

**Key Topics:**
- State update troubleshooting
- Async update debugging
- Thread panic handling
- Entity/view lifecycle issues
- Layout debugging
- Event handler fixes
- Performance optimization
- Memory leak prevention

## How to Use Skills

Skills are reference documents that provide:
- **When to Use**: Clear scenarios for applying the skill
- **Patterns**: Code examples and best practices
- **Checklists**: Step-by-step guides
- **Common Issues**: Known problems and solutions
- **Resources**: Links to documentation

### In Claude Code

When working with Claude Code, you can reference these skills:
```
"Follow the patterns in .claude/skills/gpui-style-guide when writing UI code"
"Use the add-cloud-provider skill to help me add Azure support"
"Check the security-audit skill before this release"
```

### Manual Reference

Browse the skill files directly for:
- Learning GPUI patterns
- Understanding project conventions
- Quick reference during development
- Code review guidelines

## Skill Development

### Creating New Skills

To add a new skill:

1. Create a new directory: `.claude/skills/your-skill-name/`
2. Add `SKILL.md` with the skill content
3. Include examples, patterns, and checklists
4. Update this README

### Skill Structure

Each skill should have:

```markdown
# Skill Name

Brief description

## When to Use
- Specific scenarios

## Key Concepts
- Main topics covered

## Examples
Code examples with explanations

## Checklists
Step-by-step guides

## Resources
Links to documentation
```

## Project-Specific Context

### CloudBridge Architecture

```
┌─────────────────────────────────────────────────────┐
│                 GPUI Application Layer               │
│  (CloudBridgeApp → Dashboard/Accounts/Settings)     │
└────────────┬────────────────────────────────────────┘
             │
┌────────────▼────────────────────────────────────────┐
│          UI Component Layer (gpui-component)        │
│  (Input, Button, Chart, Switch, Scroll)            │
└────────────┬────────────────────────────────────────┘
             │
┌────────────▼────────────────────────────────────────┐
│         Business Logic Layer                        │
│  ├─ Cloud Providers (AWS, Aliyun, DeepSeek)       │
│  ├─ Cost Aggregation & Filtering                   │
│  └─ Credential Validation                          │
└────────────┬────────────────────────────────────────┘
             │
┌────────────▼────────────────────────────────────────┐
│         Data Persistence Layer                      │
│  ├─ DuckDB (cost_data, cache tables)               │
│  ├─ Config File (config.json - encryption key)     │
│  └─ OS Keyring (AK/SK secrets)                      │
└────────────┬────────────────────────────────────────┘
             │
┌────────────▼────────────────────────────────────────┐
│         Security Layer                              │
│  ├─ AES-256-GCM Encryption (crypto.rs)             │
│  ├─ AWS Signature V4 / Aliyun HMAC-SHA1            │
│  └─ OS Credential Management                       │
└─────────────────────────────────────────────────────┘
```

### Key Technologies

- **UI Framework**: GPUI 0.2 (GPU-accelerated)
- **Language**: Rust 2021 Edition
- **Database**: DuckDB (embedded)
- **HTTP**: ureq (synchronous)
- **Async Runtime**: smol
- **Encryption**: AES-256-GCM

### Code Statistics

- Total Lines: ~4,868 lines of Rust
- UI Layer: 44% (1,698 lines)
- Cloud Integrations: 40% (1,538 lines)
- Core Infrastructure: 8% (303 lines)
- Supporting: 8% (329 lines)

### Known Technical Debt

1. Excessive `.clone()` usage (34+ calls)
2. Large files (dashboard.rs: 837 lines, accounts.rs: 684 lines)
3. Manual thread management with message channels
4. Limited test coverage
5. Encryption key storage with encrypted data

## Contributing

When adding or updating skills:
1. Keep examples relevant to CloudBridge
2. Use actual code from the project when possible
3. Include both good and bad examples
4. Add checklists for actionable items
5. Update this README

## Resources

- [CloudBridge Repository](https://github.com/JetSquirrel/cloudbridge)
- [GPUI Documentation](https://gpui.rs/)
- [GPUI Component Library](https://longbridge.github.io/gpui-component/)
- [Rust Book](https://doc.rust-lang.org/book/)

---

These skills are based on the patterns observed in CloudBridge and inspired by the [longbridge/gpui-component](https://github.com/longbridge/gpui-component) skills structure.
