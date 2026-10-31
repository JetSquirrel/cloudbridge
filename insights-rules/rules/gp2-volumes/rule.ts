// EBS volumes still on gp2. gp3 costs 20% less per GB in every region and
// gives every volume 3,000 IOPS and 125 MB/s as a baseline, which a gp2
// volume under 1 TB does not have. The change is made in place, online.
import type { Finding, Input, Rule } from "../../types/cloudbridge-insights";

export const rule: Rule = {
  title: "Volumes still on gp2",
  label: "GP2 VOLUMES",
  explanation:
    "EBS volumes billed as gp2. gp3 is 20% cheaper per GB with a higher baseline; " +
    "modify the volume type in place, no detach or downtime.",
  version: "1.0.0",
  author: "JetSquirrel",
  providers: ["AWS"],
  apiVersion: 1,
};

const GP2 = /\(gp2\)/i;
const SAVING = 0.2;

export function evaluate(input: Input): Finding[] {
  const findings: Finding[] = [];
  for (const r of input.billed) {
    const gp2 = r.lines.filter((l) => GP2.test(l.description ?? ""));
    const cost = gp2.reduce((sum, l) => sum + l.cost, 0);
    if (cost <= 0) continue;
    const gbMonths = gp2.reduce((sum, l) => sum + l.quantity, 0);
    findings.push({
      id: r.id,
      cost,
      evidence:
        `${gbMonths.toFixed(1)} GB-months of gp2; as gp3 about ` +
        `${(cost * SAVING).toFixed(2)} ${input.currency} less this period`,
    });
  }
  return findings;
}
