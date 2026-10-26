// Default Cedar policies for the tool sandbox.
//
// Cedar denies whatever no policy permits, so the baseline permit keeps
// today's behaviour: the capability sandbox behind this layer still owns
// the allow decision.  The forbids are what Cedar adds on top of it.

@id("baseline-permit")
permit (principal, action == Dyson::Action::"call_tool", resource);

// A download piped straight into a shell runs code nobody has read.
@id("no-pipe-to-shell")
forbid (
  principal,
  action == Dyson::Action::"call_tool",
  resource == Dyson::Tool::"bash"
)
when {
  context has command &&
  (context.command like "*curl *" || context.command like "*wget *") &&
  (context.command like "*| sh" || context.command like "*| sh *" ||
   context.command like "*|sh" || context.command like "*|sh *" ||
   context.command like "*| bash" || context.command like "*| bash *" ||
   context.command like "*|bash" || context.command like "*|bash *")
};

// Wiping the filesystem root or the home directory is never a task step.
@id("no-rm-rf-root")
forbid (
  principal,
  action == Dyson::Action::"call_tool",
  resource == Dyson::Tool::"bash"
)
when {
  context has command &&
  (context.command like "*rm -rf /" || context.command like "*rm -rf / *" ||
   context.command like "*rm -rf /\**" ||
   context.command like "*rm -rf ~" || context.command like "*rm -rf ~/" ||
   context.command like "*rm -rf ~ *" || context.command like "*rm -rf ~/ *")
};

// Private keys and cloud credentials stay out of the model's context.
@id("no-credential-file-read")
forbid (principal, action == Dyson::Action::"call_tool", resource)
when {
  resource.kind == "builtin" &&
  context has path &&
  (context.path like "*/.ssh/id_*" || context.path like "*/.aws/credentials" ||
   context.path like "*/.config/gcloud/*" || context.path like "*/.kube/config")
};

// Cloud metadata endpoints hand out the host's credentials.
@id("no-metadata-endpoint")
forbid (principal, action == Dyson::Action::"call_tool", resource)
when {
  (context has host &&
   (context.host == "169.254.169.254" || context.host == "metadata.google.internal")) ||
  (context has command &&
   (context.command like "*169.254.169.254*" || context.command like "*metadata.google.internal*"))
};
