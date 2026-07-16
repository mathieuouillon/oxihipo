import clsx from 'clsx';
import Link from '@docusaurus/Link';
import useDocusaurusContext from '@docusaurus/useDocusaurusContext';
import Layout from '@theme/Layout';
import CodeBlock from '@theme/CodeBlock';
import Heading from '@theme/Heading';
import HomepageFeatures from '@site/src/components/HomepageFeatures';

import styles from './index.module.css';

const RUST_SAMPLE = `use oxihipo::{Chain, Filter};

let chain = Chain::open("/data/run5042")?          // file | dir | glob | list
    .with_filter(Filter::require(["REC::Particle"]))?;

for ev in chain.events() {
    let p = oxihipo::or_continue!(ev?.bank("REC::Particle"));
    for r in 0..p.rows() {
        let pid: i32 = p.get("pid", r);
        let px:  f32 = p.get("px",  r);
    }
}`;

const PY_SAMPLE = `import oxihipo as ox

f = ox.open("/data/run5042")            # file | dir | glob | list
p = f.arrays("REC::Particle", ["pid", "px"])   # ak.Array: N * var * {pid, px}

# bigger than RAM? stream it in bounded chunks
for chunk in f.iterate("REC::Particle", step_size="200 MB"):
    hist.fill(ak.flatten(chunk.px))`;

function HomepageHeader() {
  const { siteConfig } = useDocusaurusContext();
  return (
    <header className={clsx('hero', styles.heroBanner)}>
      <div className="container">
        <Heading as="h1" className={styles.heroTitle}>
          {siteConfig.title}
        </Heading>
        <p className={styles.heroSubtitle}>
          A pure-Rust reader and writer for the <strong>HIPO v6</strong> container
          used at Jefferson Lab CLAS12 — built to read faster than the C++{' '}
          <code>hipo4</code> reader, with an uproot-shaped Python binding on top.
        </p>
        <div className={styles.buttons}>
          <Link className="button button--primary button--lg" to="/docs/intro">
            Get started
          </Link>
          <Link
            className="button button--secondary button--lg"
            to="/docs/performance/benchmarks">
            See the benchmarks
          </Link>
        </div>
      </div>
    </header>
  );
}

function HomepageSamples() {
  return (
    <section className={styles.samples}>
      <div className="container">
        <div className="row">
          <div className="col col--6">
            <Heading as="h3">Rust</Heading>
            <CodeBlock language="rust">{RUST_SAMPLE}</CodeBlock>
          </div>
          <div className="col col--6">
            <Heading as="h3">Python</Heading>
            <CodeBlock language="python">{PY_SAMPLE}</CodeBlock>
          </div>
        </div>
      </div>
    </section>
  );
}

function HomepageNumbers() {
  return (
    <section className={styles.numbers}>
      <div className="container">
        <Heading as="h2" className={styles.numbersTitle}>
          Measured, not claimed
        </Heading>
        <p className={styles.numbersLead}>
          A 29.7&nbsp;GB CLAS12 skim on JLab ifarm (<code>/volatile</code>, Lustre,
          64 cores), read with <code>Lz4ByBank</code> — the format extension that
          inflates only the banks an analysis actually touches.
        </p>
        <div className="row">
          <div className="col col--4">
            <div className={styles.stat}>
              <div className={styles.statValue}>36.6 Mev/s</div>
              <div className={styles.statLabel}>
                events/s at <code>par=64</code>, versus 1.4&nbsp;Mev/s for the{' '}
                <code>Lz4</code> baseline
              </div>
            </div>
          </div>
          <div className="col col--4">
            <div className={styles.stat}>
              <div className={styles.statValue}>−77.6%</div>
              <div className={styles.statLabel}>
                file size on this skim (29.7&nbsp;GB → 6.66&nbsp;GB); expect ±5% on
                generic reco files
              </div>
            </div>
          </div>
          <div className="col col--4">
            <div className={styles.stat}>
              <div className={styles.statValue}>~90%</div>
              <div className={styles.statLabel}>
                of native Rust throughput from Python — the decode runs in Rust with
                the GIL released
              </div>
            </div>
          </div>
        </div>
        <p className={styles.numbersFoot}>
          <Link to="/docs/performance/benchmarks">
            Full benchmark tables, hardware, and reproduction steps →
          </Link>
        </p>
      </div>
    </section>
  );
}

export default function Home() {
  return (
    <Layout
      title="Fast HIPO v6 in Rust"
      description="Pure-Rust reader and writer for the HIPO v6 container used at Jefferson Lab CLAS12, with a columnar, uproot-shaped Python binding.">
      <HomepageHeader />
      <main>
        <HomepageFeatures />
        <HomepageSamples />
        <HomepageNumbers />
      </main>
    </Layout>
  );
}
