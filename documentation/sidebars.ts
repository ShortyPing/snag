import type {SidebarsConfig} from '@docusaurus/plugin-content-docs';

// This runs in Node.js - Don't use client-side code here (browser APIs, JSX...)

const sidebars: SidebarsConfig = {
  docsSidebar: [
    'intro',
    {
      type: 'category',
      label: 'Getting started',
      collapsed: false,
      items: [
        'getting-started/installation',
        'getting-started/quickstart',
        'getting-started/your-first-suite',
      ],
    },
    {
      type: 'category',
      label: 'Guides',
      collapsed: false,
      items: [
        'guides/suite-files',
        'guides/writing-scripts',
        'guides/setup-and-teardown',
        'guides/variables-and-secrets',
        'guides/selecting-tests',
        'guides/execution-model',
        'guides/reporters',
        'guides/ci-integration',
        'guides/troubleshooting',
      ],
    },
    {
      type: 'category',
      label: 'Reference',
      collapsed: false,
      items: [
        'reference/cli',
        'reference/manifest',
        'reference/script-api',
        'reference/report-formats',
        'reference/exit-codes',
        'reference/revision-file',
      ],
    },
    {
      type: 'category',
      label: 'Internals',
      collapsed: false,
      items: [
        'internals/architecture',
        'internals/module-tour',
        'internals/extending-the-script-api',
        'internals/adding-a-reporter',
        'internals/testing',
        'internals/contributing',
      ],
    },
  ],
};

export default sidebars;
