-- OTel 4: error rate per route (status_code is 0 ok / 1 error, so avg() is the rate)
SELECT "attributes.http.route", avg(status_code) AS error_rate, count(*) AS total
FROM otel.spans GROUP BY "attributes.http.route" ORDER BY error_rate DESC
