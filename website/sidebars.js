// @ts-check

/** @type {import('@docusaurus/plugin-content-docs').SidebarsConfig} */
const sidebars = {
  docsSidebar: [
    'intro',
    {
      type: 'category',
      label: 'Getting started',
      collapsed: false,
      items: ['getting-started/rust', 'getting-started/python'],
    },
    {
      type: 'category',
      label: 'Rust guide',
      items: ['rust/reading', 'rust/writing', 'rust/design'],
    },
    {
      type: 'category',
      label: 'Python guide',
      items: [
        'python/reading',
        'python/writing',
        'python/rdataframe',
        'python/streaming',
        'python/parallel',
        'python/how-it-works',
      ],
    },
    {
      type: 'category',
      label: 'Performance',
      items: [
        'performance/compression',
        'performance/shared-filesystems',
        'performance/benchmarks',
      ],
    },
    {
      type: 'category',
      label: 'Design notes',
      items: [
        'design/event-tagging',
        'design/python-binding',
        'design/python-vs-rust-benchmark',
      ],
    },
  ],
};

export default sidebars;
