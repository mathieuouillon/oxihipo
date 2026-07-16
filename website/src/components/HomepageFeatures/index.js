import Heading from '@theme/Heading';
import Link from '@docusaurus/Link';
import styles from './styles.module.css';

const FeatureList = [
  {
    title: 'Zero-copy columnar reads',
    description: (
      <>
        <code>bank.col::&lt;T&gt;("name")</code> hands back a <code>Cow&lt;[T]&gt;</code>{' '}
        borrowed straight from the decompressed record buffer when the bytes are
        aligned, with a one-shot copy fallback otherwise. Fixed-length array
        columns (<code>name/T#N</code>) read as <code>[T; N]</code>.
      </>
    ),
  },
  {
    title: 'Bounded memory, any file size',
    description: (
      <>
        Records stream one at a time through a recycled buffer — the file is never
        mapped or read whole. A sequential scan of a 100&nbsp;GB file holds about
        one record resident; parallel scans hold one per worker.
      </>
    ),
  },
  {
    title: 'One reader: Chain',
    description: (
      <>
        <code>Chain::open</code> takes a file, a directory, a glob, or a list of
        paths. Multi-file chains share a single parsed dictionary, and{' '}
        <code>events()</code> yields <code>Result</code>, so a truncated record
        surfaces as an <code>Err</code> instead of a panic.
      </>
    ),
  },
  {
    title: 'Data-parallel scans',
    description: (
      <>
        <code>for_each(threads, f)</code> fans work across cores out of order —{' '}
        <code>0</code> for every core, <code>1</code> for sequential,{' '}
        <code>n</code> for exactly <code>n</code>. The thread count is the only
        difference between the two.
      </>
    ),
  },
  {
    title: 'Decompress only what you read',
    description: (
      <>
        <Link to="/docs/performance/compression">
          <code>Lz4ByBank</code>
        </Link>{' '}
        stores each bank as its own LZ4 stream and inflates one only when{' '}
        <code>ev.bank(name)</code> asks. Real analyses touch a handful of ~30
        banks — the rest stay compressed. No reader-side API change.
      </>
    ),
  },
  {
    title: 'Python that feels like uproot',
    description: (
      <>
        A HIPO bank reads like an{' '}
        <a href="https://awkward-array.org">Awkward</a> jagged branch. The
        per-event loop runs in Rust with the GIL released and columns move into
        NumPy zero-copy, so the binding costs about 10%.
      </>
    ),
  },
];

function Feature({ title, description }) {
  return (
    <div className="col col--4">
      <div className={styles.card}>
        <Heading as="h3" className={styles.cardTitle}>
          {title}
        </Heading>
        <p className={styles.cardBody}>{description}</p>
      </div>
    </div>
  );
}

export default function HomepageFeatures() {
  return (
    <section className={styles.features}>
      <div className="container">
        <div className="row">
          {FeatureList.map((props, idx) => (
            <Feature key={idx} {...props} />
          ))}
        </div>
      </div>
    </section>
  );
}
