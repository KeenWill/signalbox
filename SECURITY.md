# Security policy

Signalbox is in a design-only phase and has no supported release. Security-sensitive design includes provider credentials, remote runner identity, approval binding, audit provenance, and ambiguous external side effects.

## Reporting a vulnerability

Do not disclose a suspected vulnerability in a public issue. Use GitHub's private vulnerability reporting for this repository when available. If that channel is unavailable, contact the repository owner privately through the contact method on their GitHub profile and include:

- the affected document, future version, or component;
- the impact and conditions required to reproduce it;
- a minimal reproduction or scenario where safe; and
- any known mitigation.

Do not include live credentials, private transcripts, or data belonging to another system. Receipt and remediation timelines are not yet guaranteed while the project has no release. This policy will be revised before executable software is published.

## Scope during the foundation phase

Design flaws that could cause privilege confusion, approval reuse, stale-result acceptance, credential exposure, silent work loss, or unsafe retry are useful reports. General feature requests belong in normal project discussion.
