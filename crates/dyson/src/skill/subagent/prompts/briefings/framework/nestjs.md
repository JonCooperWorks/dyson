Starting points for NestJS (TypeScript) — not exhaustive. DI + decorators sit on top of Express or Fastify under the hood; their sinks still apply. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`@Body() dto`, `@Query() q`, `@Param('id') id`, `@Headers() h`, `@Cookies() c`, `@Req() req` / `@Res() res` (raw underlying framework request), `@UploadedFile() file`, `@UploadedFiles() files`.

NestJS validation runs via `class-validator` + `class-transformer` when `ValidationPipe` is registered globally (`app.useGlobalPipes(new ValidationPipe({ whitelist: true, forbidNonWhitelisted: true }))`).  Without `whitelist: true`, unknown fields on the DTO are kept — mass assignment surface.

## Sinks

**SQL (TypeORM / Prisma / Sequelize / MikroORM)**
- TypeORM `repo.createQueryBuilder().where(\`name = '${user}'\`).getMany()` — template literal concat: SQLi.  Use `.where('name = :name', { name: user })`.
- `connection.query(\`... ${user}\`)` — raw SQL.
- Prisma `$queryRawUnsafe(\`...${user}\`)` — SQLi; use `$queryRaw` tagged template.

**Mass assignment / DTO bypass**
- `@Body() body: any` — no validation; attacker sends any shape.  Attacker sets `admin: true`, `role: 'SUPERUSER'`, bypasses intended DTO.
- `@Body() dto: UserDto` with a DTO missing `forbidNonWhitelisted: true` globally — attacker adds extra fields; if downstream `entity.assign(dto)` / `new User(dto)`, mass assignment.
- `plainToClass(User, body, { excludeExtraneousValues: false })` — default accepts extras.

**Deserialization / prototype pollution**
- `body[user_key]` walk over attacker keys — the standard JS prototype-walk primitive.
- `Object.assign(target, req.body)` / `_.merge(config, req.body)` inside a guard or interceptor.

**Command execution**
- `child_process.exec(req.body.cmd)` in a service — RCE.
- `spawn('bash', ['-c', user])`.

**File / path**
- `@UploadedFile() file: Express.Multer.File` — `file.originalname` is attacker-supplied.  `path.join(uploadDir, file.originalname)` without `path.basename` is traversal.
- `res.sendFile(req.query.path)` when using `@Res() res`.
- `StreamableFile` with a user-derived path.

**Redirect / SSRF**
- `return { url: userUrl, statusCode: 302 }` from a controller (Nest's `@Redirect()` decorator + dynamic return) — open redirect.
- HttpService `httpService.get(userUrl)` — SSRF.

**Auth / guards**
- `@UseGuards(AuthGuard)` missing on a controller method handling sensitive ops.  Global guards (`app.useGlobalGuards(...)`) exempt routes marked `@Public()`; check no public-marker is on an endpoint that shouldn't be.
- Passport strategies: `JwtStrategy` with hardcoded `secretOrKey`.
- `passport-jwt` with `ignoreExpiration: true` — never correct.

**GraphQL (Apollo via `@nestjs/graphql`)**
- Field resolvers returning unauthenticated data because a `@UseGuards` missing on the `@Resolver`.
- N+1 auth bypass: per-item resolvers skipping the parent's auth check.
- Introspection (`playground: true` / `introspection: true`) on a production GraphQL endpoint — schema disclosure.

**WebSocket / microservices**
- `@SubscribeMessage('event')` without an auth guard — anonymous websocket clients can invoke.
- Microservice `@MessagePattern(pattern)` — if the broker is reachable by unauth producers, ANY published message triggers the handler.  Check the transport (Redis / NATS / Kafka) auth config.

**Config / secrets**
- `ConfigModule.forRoot({ isGlobal: true })` loading `.env` at runtime — fine; check `.env` isn't committed.
- `JwtModule.register({ secret: 'dev-secret' })` — hardcoded signing key.
- `process.env.JWT_SECRET || 'fallback'` — fallback literal is a committed secret.

## Tree-sitter seeds (typescript, NestJS-focused)

```scheme
; NestJS parameter decorators: @Body / @Query / @Param / @Headers / @Req / @Res
(decorator (call_expression
  function: (identifier) @d
  (#match? @d "^(Body|Query|Param|Headers|Cookies|Req|Request|Res|Response|UploadedFile|UploadedFiles)$")))

; @UseGuards / @Public / @Roles — auth-ish decorators
(decorator (call_expression
  function: (identifier) @d
  (#match? @d "^(UseGuards|Public|Roles|Auth)$")))

; @Redirect dynamic return
(decorator (call_expression
  function: (identifier) @d
  (#eq? @d "Redirect")))
```
