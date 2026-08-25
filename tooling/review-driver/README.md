# Review driver

`review_driver.py` drives one immutable pull-request review attempt through the
client-fed review boundary. It takes a GitHub `OWNER/NAME`, a positive pull
request number, and the Signalbox daemon socket:

```console
python3 tooling/review-driver/review_driver.py KeenWill/signalbox 123 \
  /run/user/1000/signalbox.sock
```

If `signalbox` is not on `PATH`, pass `--signalbox-bin /path/to/signalbox` or
set `SIGNALBOX_BIN`. The command-line option takes precedence over the
environment.

The driver reads exact pull-request facts with `gh`, derives UUIDv5 identities
from the exact head and base revisions, and supplies every mutation with a
stable command identity. Re-running the command therefore resumes the same
target and attempt. A moved head changes the identity material and creates the
new target and attempt required by the review-workflow contract.

The attempt, and every command and pass identity it owns, additionally commits
to the frozen orchestration configuration: the concern-set version, the four
stage template names, the ordered concern set, and the catalog version each
reserved template currently resolves to. Changing any of them creates the new
attempt the contract requires instead of replaying the superseded one, because
an equal payload under an unchanged command identity returns the daemon's
recorded receipt before it resolves a single template. The target is the
immutable external snapshot and stays outside that material, so one pull-request
snapshot never accumulates a target per configuration.

Every configured concern is commissioned before any terminal outcome is
collected, so one unsuccessful member neither blocks the others nor strands
their durable slots: the daemon holds the attempt at `awaiting_concerns` until
every member carries a recorded claim, and only a member recorded `failed` may
later be retried.

A terminal turn authenticates the pass, never the workflow operation. An import
turn that reaches `completed` without producing imported context is recorded as
an unsuccessful import outcome rather than advancing the attempt to concern
fan-out on the turn lifecycle alone.

The commissioned accepted input and origin turn are selected once by their
lowest acceptance position, then pinned across run creation, activation,
terminal waiting, and a final terminal recheck. Later reconciliation turns in
the same session cannot change the durable command intent.

Session-backed passes are commissioned directly over the version-one process
socket because the terminal client does not expose `commission_session`; review
mutations still use the `signalbox review` commands. No local progress file is
an authority.

The implemented daemon does not yet expose the typed structured-result adapter
required to turn a concern session into `submit_review_findings` evidence. The
driver commissions and binds those passes but exits at that boundary; it never
treats free-form transcript prose as findings. Every operational failure is one
greppable stderr line beginning with `REVIEW_DRIVER_FAILURE`.

Run its tests without a daemon or GitHub connection:

```console
python3 -m unittest discover -s tooling/review-driver -p 'test_*.py'
```
