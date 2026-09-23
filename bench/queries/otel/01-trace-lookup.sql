-- OTel 1: all spans for one trace, by its predictable id
SELECT * FROM otel.spans WHERE trace_id = '00000000000000000000000000000001'
