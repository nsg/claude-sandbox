---
name: testing
description: Testing strategy — what to test, what to leave out, where tests run, and how to prune them once the code works. Load before writing, reviewing, or removing tests, and before changing which tests run in CI.
---

# Testing Strategy

Tests exist to verify that the software we wrote works, and to let a future
agent notice when they break it. Every test costs maintenance and execution
time, so the goal is the smallest suite that still catches regressions in
functionality. Everything here is a judgment call — there is no right answer,
only a bias: fewer, sharper tests.

## Scope: only our code

- Test logic we wrote and maintain, and behavior that affects this project.
- Do not test dependencies, libraries, the language, or the framework. A test
  that would still pass with our code deleted is testing someone else's.
- Glue that only forwards to a library rarely needs a test. Our decisions
  around the library — parsing, validation, branching, error mapping — do.

## Unit tests

- Write them when the logic allows it; they are the default kind of test.
- Each one targets a single specific behavior. No broad tests that sweep
  through many things at once.
- Avoid overlap. When a new test covers a superset of an existing one, keep
  one of them — usually the more specific one, unless it adds nothing.
- Keep them trimmed and readable. A test that is hard to maintain gets
  deleted or ignored, so it protects nothing.

## Integration tests

The same rules apply, with a higher bar. Before adding one, ask whether it
actually makes sense.

- They earn their place while a feature is being built, or during a refactor
  where regressions are a real worry.
- Once the code stabilizes they are often just execution time. Remove them
  unless they guard something unit tests cannot.
- Keep the count to a relative minimum.

## Where tests run

- **Locally, in the sandbox:** run as much as possible while writing the
  software — unit tests, integration tests, whatever exists. This is where
  the full suite lives.
- **In CI:** not the entire suite. Focus on security audits, unit tests, and
  similar cheap, high-signal checks. Integration tests stay out of CI unless
  a specific case justifies it. Guard the scope and runtime of CI so it does
  not creep.

## Workflow

Think test-driven: tests are how you prove the code works. The order is yours
— code first, tests first, or both together. Use as many tests, scripts, and
local integration checks as you need to get to a working solution.

## Prune when it works

Once there is a clear, working solution, turn critical on the tests you wrote
along the way. Implementation-phase tests are scaffolding; most of them are
not needed for the long term.

For each test, script, and integration check, ask:

- Does it catch a regression in functionality that a future agent could
  plausibly cause?
- Is it already covered by another test?
- Does it test our logic, or a dependency?
- Is it worth its execution time, especially in CI?

Delete what fails these questions. What remains should be the bare minimum
that verifies the functionality and flags it when something breaks.
