Starting points for Celery (Python distributed task queue) — not exhaustive. Celery isn't a web framework — it's a job system, but its attack surface is distinct and often overlooked. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)

A Celery task's arguments are serialized messages pulled from a broker (Redis / RabbitMQ / SQS).  Who writes to the broker determines the trust level:

- Broker exposed to a web app that accepts user data and enqueues it → args are attacker-derived.
- Broker reachable by unauthenticated producers → ANY published message invokes the task.
- Result backend visible to workers but also to an attacker → task results can be a covert source.

## Sinks

**Serializer choice — THE #1 Celery CVE class**
- `CELERY_TASK_SERIALIZER = 'pickle'` / `CELERY_ACCEPT_CONTENT = ['pickle']` — pickle-based task args.  Any message on the broker becomes a pickle deserialization → RCE.  Default in old Celery versions (pre-3.x); explicit opt-out required.
- Fix: `CELERY_TASK_SERIALIZER = 'json'` / `CELERY_ACCEPT_CONTENT = ['json']`.  JSON is safe; msgpack is safe.
- Mixed: `CELERY_ACCEPT_CONTENT = ['json', 'pickle']` still accepts pickle from the broker; attacker publishes a pickled message and gets RCE.  Must be a single-item list `['json']`.

**Command execution via task body**
- Tasks calling `subprocess.run(args[0], shell=True)` on user-derived `args[0]` — RCE.
- Tasks invoking `os.system`, `eval`, `exec`.

**SQL in tasks**
- Tasks issuing raw SQL with interpolated args — same as web-handler SQLi, but without the HTTP middleware in front.

**Task routing / selection**
- `app.send_task(user_task_name, args, kwargs)` — `user_task_name` as a task name the attacker can choose + tasks that auto-register by module import means an attacker who controls the task name can invoke any registered task.
- `celery -A app.tasks.user_module worker` — if `user_module` comes from config ever user-writable at deploy time, attacker picks loaded modules.

**Broker auth / network**
- `broker_url = 'redis://:@localhost:6379/0'` — empty password.  `broker_url = 'redis://localhost:6379/0'` (no auth field) on a Redis exposed to the network = publicly controllable queue.
- `broker_url = 'amqp://guest:guest@rabbit:5672//'` — default rabbit creds.
- Hardcoded broker credentials in committed source (`config.py` / `settings.py`).

**Result backend auth**
- `result_backend = 'redis://localhost:6379/1'` — result backend on the same unauthenticated Redis.  Task outputs (often sensitive) readable by the network.
- `result_backend = 'db+mysql://...'` — DB-backed results; SQL credentials in source are a finding.

**`shared_task` + Django integration**
- `@shared_task` running in a Django context; `user_id` passed in argments without verifying the user still has access at task execution time = TOCTOU / IDOR at task dispatch.

**Periodic / beat tasks**
- `app.conf.beat_schedule` — periodic tasks registered at app import.  Attacker-controlled `beat_schedule` (via admin UI or config reload) can register new tasks / crontabs.  Usually internal, but worth flagging if the schedule is reloaded from user-writable state.

**Sensitive data in logs**
- Celery logs task args by default at INFO level.  Tasks receiving tokens / credentials in args → credential leak to log aggregators.  Set `task_ignore_result` / custom `Task.on_failure` to scrub.

## Tree-sitter seeds (python, Celery-focused)

```scheme
; @app.task / @shared_task decorators
(decorator (call
  function: (attribute
    attribute: (identifier) @m)
  (#match? @m "^(task|shared_task)$")))

(decorator (identifier) @d (#eq? @d "shared_task"))

; .delay(...) / .apply_async(...) / .send_task(...)
(call function: (attribute
    attribute: (identifier) @m)
  (#match? @m "^(delay|apply_async|send_task|apply)$"))
```
