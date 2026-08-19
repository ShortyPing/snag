import type {ReactNode} from 'react';
import clsx from 'clsx';
import Link from '@docusaurus/Link';
import CodeBlock from '@theme/CodeBlock';
import Heading from '@theme/Heading';
import Layout from '@theme/Layout';
import useDocusaurusContext from '@docusaurus/useDocusaurusContext';
import HomepageFeatures from '@site/src/components/HomepageFeatures';

import styles from './index.module.css';

const SUITE_SAMPLE = `title = "Checkout API"
timeout = "10s"

[variables]
base_url = "https://api.example.com"

[[test]]
id = "cart-create"
name = "POST /carts creates a cart"
tags = ["smoke"]
file = "./cart_create.snag"`;

const SCRIPT_SAMPLE = `let res = post(\`\${base_url}/carts\`)
    .header("authorization", basic("demo", env("API_PASSWORD")))
    .json(#{ currency: "EUR" })
    .send();

assert_status(res, 201);
assert_faster_than(res, 800);
assert_eq(field(res.json(), "currency"), "EUR");`;

function Terminal(): ReactNode {
  return (
    <div className="snagTerminal">
      <div className="snagTerminal__bar">
        <span className="snagTerminal__dot" />
        <span className="snagTerminal__dot" />
        <span className="snagTerminal__dot" />
      </div>
      <pre className="snagTerminal__body">
        <span className="snagTerminal__prompt">$</span> snag{'\n'}
        running 3 tests across 1 suite{'\n'}
        <span className="snagTerminal__pass">PASS</span> POST /carts creates a
        cart <span className="snagTerminal__muted">[142ms]</span>
        {'\n'}
        <span className="snagTerminal__pass">PASS</span> GET /carts/:id returns
        the cart <span className="snagTerminal__muted">[96ms]</span>
        {'\n'}
        <span className="snagTerminal__fail">FAIL</span> DELETE /carts/:id is
        idempotent <span className="snagTerminal__muted">[88ms]</span>
        {'\n\n'}
        failures:{'\n\n'}
        {'  '}DELETE /carts/:id is idempotent (suite.toml::cart-delete){'\n'}
        {'    '}assertion failed: expected status 204, got 500{'\n\n'}
        test result: <span className="snagTerminal__fail">FAILED</span>. 2
        passed; 1 failed; 0 timed out; 0 skipped; finished in 331ms
      </pre>
    </div>
  );
}

function Hero(): ReactNode {
  const {siteConfig} = useDocusaurusContext();
  return (
    <header className={clsx('snagHero', styles.hero)}>
      <div className="container">
        <div className={styles.heroGrid}>
          <div>
            <Heading as="h1" className={styles.heroTitle}>
              {siteConfig.title}
            </Heading>
            <p className={styles.heroTagline}>
              A regression test runner for HTTP APIs. Suites are declared in
              TOML, assertions are written in Rhai, and results come out in
              whatever format the consumer speaks.
            </p>
            <div className={styles.heroButtons}>
              <Link
                className="button button--lg button--warning"
                to="/docs/getting-started/quickstart">
                Quickstart
              </Link>
              <Link
                className="button button--lg button--outline button--secondary"
                to="/docs/intro">
                Read the docs
              </Link>
            </div>
            <p className={styles.heroMeta}>
              Single static binary · Rust 2024 edition · exit code 0 / 1 / 2
            </p>
          </div>
          <Terminal />
        </div>
      </div>
    </header>
  );
}

function Sample(): ReactNode {
  return (
    <section className={styles.sample}>
      <div className="container">
        <Heading as="h2" className={styles.sampleTitle}>
          A suite is two files
        </Heading>
        <p className={styles.sampleLead}>
          The manifest says <em>what</em> to run; the script says{' '}
          <em>what must be true</em>. Nothing else is required.
        </p>
        <div className={styles.sampleGrid}>
          <CodeBlock language="toml" title="suite.toml">
            {SUITE_SAMPLE}
          </CodeBlock>
          <CodeBlock language="js" title="cart_create.snag">
            {SCRIPT_SAMPLE}
          </CodeBlock>
        </div>
      </div>
    </section>
  );
}

function NextSteps(): ReactNode {
  const links = [
    {
      to: '/docs/getting-started/installation',
      label: 'Install Snag',
      text: 'Build from source with cargo, or drop the binary on your PATH.',
    },
    {
      to: '/docs/guides/writing-scripts',
      label: 'Write assertions',
      text: 'Requests, responses, JSON paths, and the full assertion set.',
    },
    {
      to: '/docs/guides/ci-integration',
      label: 'Wire up CI',
      text: 'JUnit for GitHub Actions, TeamCity messages for IntelliJ.',
    },
    {
      to: '/docs/internals/architecture',
      label: 'Read the internals',
      text: 'Discovery, the worker pool, and the reporter event stream.',
    },
  ];

  return (
    <section className={styles.next}>
      <div className="container">
        <Heading as="h2" className={styles.sampleTitle}>
          Where to next
        </Heading>
        <div className={styles.nextGrid}>
          {links.map((link) => (
            <Link key={link.to} to={link.to} className={styles.nextCard}>
              <span className={styles.nextLabel}>{link.label}</span>
              <span className={styles.nextText}>{link.text}</span>
            </Link>
          ))}
        </div>
      </div>
    </section>
  );
}

export default function Home(): ReactNode {
  return (
    <Layout
      title="HTTP API regression testing"
      description="Snag is a regression test runner for HTTP APIs: TOML suites, Rhai assertions, and reports for terminals, CI, and IDEs.">
      <Hero />
      <main>
        <HomepageFeatures />
        <Sample />
        <NextSteps />
      </main>
    </Layout>
  );
}
