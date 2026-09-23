-- OTel 2: log body search by substring (MATCH() is adelie-only, §9; waits for E10)
SELECT count(*) FROM otel.logs WHERE body LIKE '%zzq_needle_7f3a%'
