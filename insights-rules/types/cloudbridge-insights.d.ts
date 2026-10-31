// Types for CloudBridge Insights rules.
//
// A rule is an ES module the desktop app reads from its data directory's
// `insights/` folder:
//   macOS    ~/Library/Application Support/CloudBridge/insights/
//   Windows  %APPDATA%\CloudBridge\data\insights\
// It is read again each time the Insights page loads. Community rules are
// installed there from Settings -> Insights rules; your own can be dropped
// in by hand.
//
// The rule runs in QuickJS with no module loader, no file or network
// access, 64 MiB of heap and two seconds. It sees only `Input`.

/** The version of `Input` and `Finding` this file describes. */
export type ApiVersion = 1;

/** What the rule is called, and what the catalog says about it. */
export interface Rule {
  /** Section heading. Defaults to the file name. */
  title?: string;
  /** Stat card label. Defaults to the title in upper case. */
  label?: string;
  /** One or two sentences under the heading: what it found and what to do. */
  explanation?: string;
  /** The rule's own version, `MAJOR.MINOR.PATCH`. Required in the catalog. */
  version?: string;
  /** GitHub handle or name. Required in the catalog. */
  author?: string;
  /** The billing sources it reads, as the bill names them: `AWS`, `Azure`, ... */
  providers?: string[];
  /** The `Input` version it was written for. A newer one than the app has is not run. */
  apiVersion?: ApiVersion;
}

export interface Input {
  apiVersion: ApiVersion;
  /** The billing period, `YYYY-MM`. */
  period: string;
  /** The reporting currency every cost is in. */
  currency: string;
  /** The last resource scan, if one was imported. */
  scan: {
    scannedAt: string;
    regions: string[];
    /**
     * `false` for a partial scan: some service or type reported errors, so
     * a billed resource missing from `resources` may only be one it did not reach.
     */
    complete: boolean;
  } | null;
  /** The scanned inventory. */
  resources: Resource[];
  /** Every resource the period's bill names, with its charge lines. */
  billed: Billed[];
}

export interface Resource {
  provider: string;
  accountId: string | null;
  id: string;
  arn: string | null;
  /** The scanner's type, e.g. `ec2:instance`. */
  type: string;
  region: string | null;
  name: string | null;
  tags: Record<string, string>;
  /** The resource's configuration, when the scan read it. */
  properties: Record<string, unknown> | null;
  /** This period's usage cost matched to it from the bill. */
  cost: number;
  /** When the provider says it was created, RFC 3339; `null` when it does not say. */
  createdAt: string | null;
  /** When the provider says it last changed, RFC 3339; `null` when it does not say. */
  modifiedAt: string | null;
}

export interface Billed {
  provider: string;
  accountId: string | null;
  /** The bill's resource id: often an ARN. */
  id: string;
  name: string | null;
  service: string;
  region: string | null;
  /** This period's usage cost. */
  cost: number;
  /** Index into `Input.resources` of the scanned resource, if matched. */
  resource: number | null;
  /** One line per charge description and unit, summed over the period. */
  lines: ChargeLine[];
}

export interface ChargeLine {
  /** The bill's description, e.g. `$0.045 per NAT Gateway Hour`. */
  description: string | null;
  unit: string | null;
  quantity: number;
  cost: number;
}

/** One resource the rule flags. Omitted fields come from the bill or scan. */
export interface Finding {
  /** A `Billed.id` or `Resource.id`/`arn`. */
  id: string;
  /** What the finding rests on, in a sentence. */
  evidence: string;
  /** Defaults to the resource's cost this period. */
  cost?: number;
  provider?: string;
  name?: string;
  /** Resource type shown in the grouping. */
  kind?: string;
  region?: string;
  accountId?: string;
}

export type Evaluate = (input: Input) => Finding[] | Promise<Finding[]>;

/** A test case: `rules/<id>/fixtures/<case>.json`. */
export interface Fixture {
  /** What the case shows, in a sentence. */
  description: string;
  input: Input;
  /** The findings expected, by id; `evidence` is compared when given. */
  expect: { id: string; evidence?: string }[];
}
