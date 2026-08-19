import type {ReactNode} from 'react';
import Heading from '@theme/Heading';
import styles from './styles.module.css';

type Feature = {
  title: string;
  body: ReactNode;
};

const FEATURES: Feature[] = [
  {
    title: 'Declarative suites',
    body: (
      <>
        A suite is a TOML file: variables, tags, timeouts, and one entry per
        test. No test-harness boilerplate, no build step — the manifest is the
        contract between your repo and the runner.
      </>
    ),
  },
  {
    title: 'Real assertions, not JSON matchers',
    body: (
      <>
        Assertions are <a href="https://rhai.rs">Rhai</a> scripts, so a test can
        branch, loop, chain requests, and walk a decoded body by dotted path —
        while still failing with one readable message.
      </>
    ),
  },
  {
    title: 'Reports for whoever is watching',
    body: (
      <>
        The same run renders as a colored terminal report, JSON, JSONL,
        TeamCity service messages (IntelliJ draws a native test tree), or JUnit
        XML for the rest of CI.
      </>
    ),
  },
  {
    title: 'Parallel by default, serial when it matters',
    body: (
      <>
        Tests run across a worker pool sized to your CPUs. Mark a test{' '}
        <code>parallel_safe = false</code> and it runs alone, before the pool
        starts.
      </>
    ),
  },
  {
    title: 'Timeouts that actually fire',
    body: (
      <>
        A timeout caps both the HTTP client and the script interpreter, so a
        hung socket and a runaway loop both end the test instead of the run.
      </>
    ),
  },
  {
    title: 'Built for CI ergonomics',
    body: (
      <>
        Filter by name, id, or tag; retry flaky tests and see the attempt count;
        shuffle with a seed for reproducible ordering; exit codes distinguish
        “tests failed” from “could not run”.
      </>
    ),
  },
];

export default function HomepageFeatures(): ReactNode {
  return (
    <section className={styles.features}>
      <div className="container">
        <Heading as="h2" className={styles.sectionTitle}>
          What Snag gives you
        </Heading>
        <div className={styles.grid}>
          {FEATURES.map((feature) => (
            <div key={feature.title} className={styles.card}>
              <Heading as="h3" className={styles.cardTitle}>
                {feature.title}
              </Heading>
              <p className={styles.cardBody}>{feature.body}</p>
            </div>
          ))}
        </div>
      </div>
    </section>
  );
}
