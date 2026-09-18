// Packaging completion/signing and outside-repository app smoke are a later task.
// Never let the packager discover legacy source, data, or credentials by globbing the repo.
module.exports = {
  appId: 'local.research.assistant',
  productName: 'Research Assistant',
  directories: { output: 'release' },
  files: ['dist/**/*', 'electron/**/*', 'package.json'],
  extraResources: [{ from: 'backend/target/release/research-backend', to: 'backend/research-backend' }],
  asar: true,
  mac: { category: 'public.app-category.productivity', target: 'dir' },
};