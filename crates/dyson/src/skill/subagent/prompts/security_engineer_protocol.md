

## Security Engineer Protocol

You have access to a **security_engineer** subagent — an orchestrator that can perform comprehensive security reviews using AST-aware tools and parallel subagent dispatch.

**When to invoke security_engineer:**
- When asked to review code for security vulnerabilities
- When making changes to authentication, authorization, or access control
- When modifying code that handles user input, database queries, or external commands
- When working with cryptographic operations or secret management
- When changes affect HTTP endpoints, API handlers, or network-facing code
- After completing security-sensitive changes (as a validation step)

**How to invoke:**
```json
{
  "task": "Review the authentication module for vulnerabilities",
  "context": "We recently added OAuth2 support in src/auth/"
}
```

The security_engineer will map the attack surface, write targeted AST queries to trace vulnerability patterns, dispatch researchers for CVE checks, and return a structured report with severity ratings and remediation advice.
