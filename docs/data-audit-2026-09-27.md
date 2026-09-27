# Data audit findings and forward fixes

The September 27 audit confirmed that the displayed 140°F Portland reading came from a bad upstream decoded value. The Celsius-to-Fahrenheit conversion was correct.

For KPWM at `2026-09-24T18:51:00Z`, AWC XML supplied `60°C`, but the raw report's precise `T01890056` group supplied `18.9°C` and a `5.6°C` dewpoint. The archive retained the decoded value, and the daily maximum selected it. The raw wind token was malformed. Decoder confusion from that token is a possible mechanism, not a confirmed upstream diagnosis.

The audit also found lost reports between hourly latest-cache snapshots, mixed forecast intervals, and settlement outcomes based on incomplete retained readings. These defects require separate controls.

## Scope and limits

The observation audit inspected 183 Parquet files from the selected September 20–27 publication listing: 456,755 rows and 447,577 distinct station/report instants. The forecast sample covered eight files and 4,918,798 rows. It did not cover the complete forecast archive.

The settlement audit checked 97 signed events and recomputed 289 entry base scores. Stored scores and announced outcomes were internally consistent. Internal consistency did not establish source completeness.

Five signed refund outcomes omitted reports found in historical source responses. Recalculation with those reports selected a single winner in each case. Source receipt times preceded the signing deadlines, but the audit could not establish the exact historical HTTP responses available to the daemon. The associated public pages advertised 15,000 sats in total pools; that is exposure requiring reconciliation, not proven payment loss.

Signed events remain unchanged. This work does not alter payments or reissue attestations.

## Implemented controls

| Defect | Forward behavior |
| --- | --- |
| Decoded fields disagree with raw METAR | Preserve the source and decoded value; reject the report with a reason. |
| Latest-cache snapshots omit reports | Collect bounded overlapping history, including routine METAR and SPECI reports. |
| Empty results hide collection failure | Retain collection receipts and distinguish successful empty responses from failed requests. |
| Forecast metrics acquire another metric's period | Preserve each source's exact native period and its extraction metadata. |
| Calendar grouping creates inverted ranges | Suppress contradictory displayed ranges and expose their source periods and warning. |
| Unit conversion or aggregation hides invalid input | Normalize supported units before aggregation and retain rejected/unverified counts. |
| SPECI prefixes double-count hourly precipitation | Construct non-overlapping accumulation intervals. |
| METAR minutes do not align with forecast hours | Collect and replay fixed UTC-hour RR7 precipitation where the official feed supports the station. |
| Partial precipitation periods require guessing | Keep the metric unavailable for that window; expose native intervals for planning. |
| Missing inputs become a signed refund | Require every enabled station/metric pair, complete coverage, and fresh collection before signing. |
| Humidity compares different statistics | Use the maximum report humidity against the forecast maximum for settlement. |
| Concurrent settlement saves inconsistent records | Commit the signature, readings, and all entry scores in one transaction; reject later writes to signed events. |
| Failures disappear in logs | Persist the settlement hold and display it in the event API and interface. |

Precipitation remains eligible. The reader requires exact interval coverage and defensible phase evidence. A closing routine report is required for positive rainfall so later phase remarks are included. It does not move an announced boundary, prorate an unknown partial total, add overlapping accumulations, or invent snowfall from a fixed liquid-to-snow ratio.

Old files without source evidence remain provisional display data. They cannot authorize a new signature merely because their numeric values look plausible. A later validated correction can replace a rejected report for an unsigned event.

## Deployment and remaining limits

Deploy the daemon and oracle together. Older readers do not enforce appended quality columns or evidence footers. New derived forecast cache versions prevent reuse of the old interval layout. The settlement-block database migration is additive.

Use [the ingestion guide](observation-ingestion.md) to check collection bounds and source retention. Use [the settlement procedure](settlement-operations.md) to check proposed windows and investigate held events. Verify sufficient overlap for the signing delay and collection latency before relying on the first new events.

The broad quality thresholds can flag real extreme weather. They are review signals, not replacement values. Regional and seasonal climatology, neighboring-station agreement, and operator alert routing remain future work.

Each published artifact retains source evidence, but the event record does not yet freeze the selected file hashes and query-policy version beside the signature. Preserve the source archive for replay. Forecast settlement verifies retained document hashes and extraction metadata; independent DWML re-extraction is covered by producer fixtures, not repeated at signing.

Raw-source consistency cannot establish physical truth when the source itself contains a plausible but incorrect observation. This release closes the identified ingestion, interval, coverage, and missing-input paths; it does not certify all weather measurements.
