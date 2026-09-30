-- How much of an AWS bill a corkscrew inventory can name: the share of
-- usage cost whose ResourceId matches a scanned resource, exactly or after
-- normalizing both sides to the id's last segment. The measurement behind
-- the "Next — resources" section of docs/roadmap.md.
--
-- Run from a directory holding
--   focus/     the FOCUS 1.2 export as delivered (data/billing_period=*/*.csv.gz)
--   corkscrew-scan.duckdb   a `corkscrew scan --provider aws` of the same account
-- with
--   duckdb -c ".read /path/to/cloudbridge/scripts/match-inventory.sql"
--
-- Both are opened read-only. A Parquet export needs read_parquet in place
-- of read_csv below.

ATTACH 'corkscrew-scan.duckdb' AS ck (READ_ONLY);

-- The resource part of an ARN (after arn:partition:service:region:account:),
-- and its last path segment: 'function:foo' -> 'foo', 'table/T' -> 'T',
-- 'key/uuid' -> 'uuid'. A bare id is its own tail.
CREATE OR REPLACE MACRO rid_tail(id) AS
    regexp_extract(regexp_replace(id, '^arn:[^:]*:[^:]*:[^:]*:[^:]*:', ''), '([^/:|]+)$', 1);

CREATE OR REPLACE TEMP TABLE bill AS
SELECT ResourceId AS rid, ServiceName AS service, RegionId AS region,
       sum(CAST(NULLIF(BilledCost, '') AS DOUBLE)) AS cost
FROM read_csv('focus/**/*.csv.gz', all_varchar = true)
WHERE ChargeCategory = 'Usage' AND coalesce(ResourceId, '') <> ''
GROUP BY ALL;

-- Every identity any scan observed: the id, the arn column (which falls
-- back to the id when a scanner has no ARN), and the tail of each.
CREATE OR REPLACE TEMP TABLE inv AS
SELECT DISTINCT resource_id, arn, type, location FROM ck.resource_observations
WHERE provider = 'aws';

CREATE OR REPLACE TEMP TABLE matched AS
SELECT b.*,
       CASE
           WHEN EXISTS (SELECT 1 FROM inv i WHERE i.resource_id = b.rid OR i.arn = b.rid) THEN 'exact'
           WHEN EXISTS (SELECT 1 FROM inv i
                        WHERE rid_tail(b.rid) IN (rid_tail(i.resource_id), rid_tail(i.arn))
                          AND (b.region IN ('global', '') OR b.region IS NULL
                               OR i.location IS NULL OR i.location IN ('global', '') OR i.location = b.region))
               THEN 'tail'
           ELSE 'none'
       END AS how
FROM bill b;

.print '== Inventory size'
SELECT count(*) AS resources, count(DISTINCT type) AS types,
       count(*) FILTER (WHERE arn LIKE 'arn:%') AS with_real_arn
FROM inv;

.print '== Match rate by usage cost'
SELECT how, count(*) AS resources, round(sum(cost), 4) AS cost,
       round(100 * sum(cost) / (SELECT sum(cost) FROM matched), 1) AS pct_cost
FROM matched GROUP BY how ORDER BY how;

.print '== By service'
SELECT service,
       round(sum(cost), 4) AS cost,
       round(100 * sum(cost) FILTER (WHERE how <> 'none') / nullif(sum(cost), 0), 1) AS pct_matched,
       count(*) FILTER (WHERE how = 'none') AS unmatched_ids
FROM matched GROUP BY service ORDER BY cost DESC;

.print '== Costliest unmatched ids'
SELECT rid, service, region, round(cost, 4) AS cost FROM matched
WHERE how = 'none' ORDER BY cost DESC LIMIT 15;

.print '== Tail collisions: one bill tail matching several inventory rows'
SELECT b.rid, count(*) AS candidates
FROM matched b JOIN inv i ON rid_tail(b.rid) IN (rid_tail(i.resource_id), rid_tail(i.arn))
WHERE b.how = 'tail' GROUP BY b.rid HAVING count(*) > 1 ORDER BY candidates DESC LIMIT 10;
