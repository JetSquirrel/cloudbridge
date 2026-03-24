# Security Audit Skill

This skill helps perform security audits on CloudBridge code and identify potential vulnerabilities.

## When to Use

Use this skill when:
- Before releases to check for security issues
- Reviewing new code for vulnerabilities
- Implementing security-sensitive features
- Responding to security reports

## Security Checklist

### 1. Credential Management

#### Current Implementation Review

```bash
# Check for hardcoded credentials
rg -i "password|secret|key|token" --type rust -g "!*.md"

# Check for exposed secrets in git history
git log -p | rg -i "password|secret|key"
```

#### Best Practices

- [ ] Credentials encrypted at rest (AES-256-GCM) ✓
- [ ] Credentials never logged
- [ ] Credentials cleared from memory when possible
- [ ] No credentials in error messages
- [ ] Encryption keys stored separately from data
- [ ] OS keyring used for sensitive data ✓

#### Vulnerabilities to Check

```rust
// BAD: Logging credentials
println!("Using key: {}", secret_key);

// BAD: Exposing in error messages
Err(format!("Authentication failed with key: {}", key))

// BAD: Storing plaintext
struct Config {
    api_key: String,  // Should be encrypted
}

// GOOD: Encrypted storage
let encrypted_key = encrypt(&api_key, &encryption_key)?;
store_credential("api_key", &encrypted_key)?;
```

### 2. Encryption Security

#### Audit Points

```rust
// Check encryption implementation
// src/crypto.rs

// ✓ GOOD: Using AES-256-GCM (authenticated encryption)
// ✓ GOOD: Random nonces
// ✗ CONCERN: Key stored with encrypted data (config.json + database)

// Improvements:
// 1. Derive key from user password + salt
// 2. Store key in OS keyring only
// 3. Use key rotation
```

#### Encryption Checklist

- [ ] Use authenticated encryption (GCM mode) ✓
- [ ] Generate random nonces/IVs ✓
- [ ] Use crypto libraries (don't roll your own) ✓
- [ ] Key length appropriate (256 bits for AES) ✓
- [ ] Keys not hardcoded ✓
- [ ] Keys separated from encrypted data ⚠️ (needs improvement)

### 3. Input Validation

#### Check All User Inputs

```rust
// src/ui/accounts.rs

// Validate account names
fn validate_account_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Name cannot be empty".to_string());
    }
    if name.len() > 100 {
        return Err("Name too long".to_string());
    }
    // Check for invalid characters
    if name.contains(|c: char| !c.is_alphanumeric() && c != '-' && c != '_') {
        return Err("Name contains invalid characters".to_string());
    }
    Ok(())
}

// Validate credentials format
fn validate_aws_credentials(ak: &str, sk: &str) -> Result<(), String> {
    if ak.len() < 16 || ak.len() > 128 {
        return Err("Invalid access key format".to_string());
    }
    if sk.len() < 16 || sk.len() > 128 {
        return Err("Invalid secret key format".to_string());
    }
    Ok(())
}
```

#### Input Validation Checklist

- [ ] All user inputs validated
- [ ] Length limits enforced
- [ ] Format validation (regex, patterns)
- [ ] Character whitelisting
- [ ] SQL injection prevention (parameterized queries) ✓
- [ ] XSS prevention (not applicable for desktop app)
- [ ] Path traversal prevention

### 4. API Security

#### HTTP Request Security

```rust
// Check all HTTP requests in cloud providers

// ✓ GOOD: Using HTTPS
// ✓ GOOD: Request signing (AWS Signature V4, Aliyun HMAC)
// ⚠️ CHECK: Certificate validation
// ⚠️ CHECK: Timeout handling
// ⚠️ CHECK: Rate limiting

// Add timeout to all requests
let response = ureq::get(url)
    .timeout(std::time::Duration::from_secs(30))
    .call()?;

// Validate response status
if response.status() != 200 {
    return Err(format!("API error: {}", response.status()));
}
```

#### API Security Checklist

- [ ] All API calls use HTTPS ✓
- [ ] Certificate validation enabled
- [ ] Timeouts configured
- [ ] Rate limiting implemented
- [ ] Retry logic with backoff
- [ ] Error responses sanitized

### 5. Error Handling

#### Secure Error Handling

```rust
// BAD: Exposing sensitive info
fn decrypt_data(data: &str, key: &[u8]) -> Result<String, String> {
    cipher.decrypt(data)
        .map_err(|e| format!("Decryption failed with key {:?}: {}", key, e))
        //                                              ^^^ Exposes key!
}

// GOOD: Generic error messages
fn decrypt_data(data: &str, key: &[u8]) -> Result<String, String> {
    cipher.decrypt(data)
        .map_err(|_| "Decryption failed".to_string())
        // Or log detailed error separately for debugging
}
```

#### Error Handling Checklist

- [ ] No sensitive data in error messages
- [ ] Generic errors to users, detailed logs for developers
- [ ] Error messages don't reveal system internals
- [ ] Stack traces not exposed to users
- [ ] Graceful degradation on errors

### 6. Dependency Security

#### Check Dependencies

```bash
# Run cargo audit
cargo audit

# Check for outdated dependencies
cargo outdated

# Check for unsafe code in dependencies
cargo geiger
```

#### Update Cargo.toml

```toml
[dependencies]
# Pin major versions to avoid breaking changes
gpui = "0.2"  # Not "0.2.*" or "*"

# Use specific versions for security-critical deps
aes-gcm = "0.10"  # Encryption
ring = "0.17"     # Cryptography

# Avoid git dependencies in production
# Some-dep = { git = "..." }  # BAD
```

#### Dependency Checklist

- [ ] Run `cargo audit` regularly ✓ (in CI)
- [ ] Review all dependencies
- [ ] Pin versions
- [ ] Avoid git dependencies
- [ ] Check for known vulnerabilities
- [ ] Review dependency licenses

### 7. Memory Safety

#### Check for Unsafe Code

```bash
# Find all unsafe blocks
rg "unsafe" --type rust

# Should be ZERO unsafe blocks in CloudBridge
# ✓ GOOD: No unsafe code found
```

#### Memory Safety Checklist

- [ ] No `unsafe` blocks ✓
- [ ] No raw pointers
- [ ] No manual memory management
- [ ] Use Rust's ownership system ✓
- [ ] Avoid panics in production code
- [ ] Handle all `Result` and `Option` properly

### 8. Thread Safety

#### Check Concurrent Access

```rust
// Check all shared state access

// BAD: Unsynchronized access
static mut COUNTER: i32 = 0;
unsafe { COUNTER += 1; }

// GOOD: Using Mutex
use std::sync::Mutex;
static COUNTER: Mutex<i32> = Mutex::new(0);
*COUNTER.lock().unwrap() += 1;

// GOOD: Using Arc for shared ownership
let data = Arc::new(Mutex::new(vec![]));
let data_clone = Arc::clone(&data);
thread::spawn(move || {
    let mut d = data_clone.lock().unwrap();
    d.push(1);
});
```

#### Thread Safety Checklist

- [ ] All shared state protected (Mutex, RwLock)
- [ ] No data races
- [ ] Send/Sync bounds respected
- [ ] No unsafe thread operations

### 9. Database Security

#### Check SQL Queries

```rust
// src/db.rs

// ✓ GOOD: Using parameterized queries
conn.execute(
    "INSERT INTO accounts (name, provider) VALUES (?, ?)",
    params![name, provider],
)?;

// BAD: String concatenation (SQL injection risk)
let query = format!("SELECT * FROM accounts WHERE name = '{}'", name);
conn.execute(&query, [])?;  // NEVER DO THIS
```

#### Database Security Checklist

- [ ] All queries parameterized ✓
- [ ] No SQL injection vulnerabilities ✓
- [ ] Database file permissions restricted
- [ ] Encrypted credentials in database ✓
- [ ] Regular backups with encryption
- [ ] Input sanitization before queries

### 10. File System Security

#### Check File Operations

```rust
// Check all file reads/writes

// Validate paths to prevent directory traversal
fn safe_config_path(filename: &str) -> Result<PathBuf, String> {
    // Sanitize filename
    if filename.contains("..") || filename.contains("/") || filename.contains("\\") {
        return Err("Invalid filename".to_string());
    }

    let config_dir = get_config_dir()?;
    let path = config_dir.join(filename);

    // Ensure path is within config directory
    if !path.starts_with(&config_dir) {
        return Err("Path traversal detected".to_string());
    }

    Ok(path)
}
```

#### File System Checklist

- [ ] Path traversal prevention
- [ ] File permissions properly set
- [ ] Temp files cleaned up
- [ ] No sensitive data in temp files
- [ ] Config files not world-readable

## Security Testing

### Manual Testing

```bash
# Test with invalid inputs
# - Empty strings
# - Very long strings (> 10KB)
# - Special characters
# - SQL injection patterns: ' OR 1=1--
# - Path traversal: ../../etc/passwd

# Test with invalid credentials
# - Wrong format
# - Expired credentials
# - Revoked credentials

# Test error conditions
# - Network failures
# - API errors
# - Database errors
# - File system errors
```

### Automated Testing

```rust
#[cfg(test)]
mod security_tests {
    use super::*;

    #[test]
    fn test_sql_injection_prevention() {
        let malicious_name = "'; DROP TABLE accounts; --";
        let result = validate_account_name(malicious_name);
        assert!(result.is_err());
    }

    #[test]
    fn test_path_traversal_prevention() {
        let malicious_path = "../../etc/passwd";
        let result = safe_config_path(malicious_path);
        assert!(result.is_err());
    }

    #[test]
    fn test_xss_prevention() {
        let malicious_input = "<script>alert('xss')</script>";
        let result = validate_account_name(malicious_input);
        assert!(result.is_err());
    }

    #[test]
    fn test_encryption_key_not_logged() {
        // Ensure encryption key is never in logs
        // This test would check log output
    }
}
```

## Common Vulnerabilities (OWASP Top 10)

### 1. Broken Access Control
- [ ] Users can only access their own accounts ✓
- [ ] No privilege escalation possible ✓
- [ ] API calls authenticated ✓

### 2. Cryptographic Failures
- [ ] Strong encryption algorithms ✓
- [ ] Proper key management ⚠️ (needs improvement)
- [ ] TLS for all network traffic ✓

### 3. Injection
- [ ] SQL injection prevented ✓
- [ ] Command injection prevented ✓
- [ ] No eval() or similar

### 4. Insecure Design
- [ ] Security considered in design
- [ ] Threat modeling performed
- [ ] Secure by default

### 5. Security Misconfiguration
- [ ] Secure defaults ✓
- [ ] No debug info in production
- [ ] Minimal attack surface

### 6. Vulnerable Components
- [ ] Dependencies audited ✓
- [ ] Regular updates ✓
- [ ] No known vulnerabilities

### 7. Authentication Failures
- [ ] Strong credential validation ✓
- [ ] Credentials encrypted ✓
- [ ] Session management (N/A for desktop)

### 8. Software & Data Integrity
- [ ] Code signing (planned)
- [ ] Dependency verification
- [ ] Update validation

### 9. Logging & Monitoring
- [ ] Security events logged
- [ ] No sensitive data in logs
- [ ] Audit trail maintained

### 10. Server-Side Request Forgery
- [ ] URL validation
- [ ] No user-controlled URLs
- [ ] Whitelist of allowed domains

## Action Items

Based on audit, prioritize fixes:

1. **High Priority** (Security Critical)
   - Separate encryption key from data
   - Add key derivation from password
   - Implement key rotation

2. **Medium Priority** (Security Enhancement)
   - Add certificate pinning
   - Implement rate limiting
   - Add request timeouts

3. **Low Priority** (Defense in Depth)
   - Add additional input validation
   - Enhance error messages
   - Add security logging

## Tools

```bash
# Install security tools
cargo install cargo-audit
cargo install cargo-outdated
cargo install cargo-geiger

# Run security checks
cargo audit
cargo outdated
cargo geiger

# Check for secrets in code
cargo install ripgrep
rg -i "password|secret|key|token" --type rust
```

## Resources

- OWASP Top 10: https://owasp.org/www-project-top-ten/
- Rust Security: https://anssi-fr.github.io/rust-guide/
- Cargo Audit: https://github.com/RustSec/rustsec
