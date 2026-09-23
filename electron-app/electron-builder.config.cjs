// Never let the packager discover legacy source, data, or credentials by globbing the repo.
module.exports = {
  appId: 'local.research.assistant',
  productName: 'Research Assistant',
  directories: { output: 'release' },
  files: ['dist/**/*', 'electron/**/*', 'package.json', '!node_modules/**/*'],
  extraResources: [{ from: 'backend/target/release/research-backend', to: 'backend/research-backend' }],
  asar: true,
  npmRebuild: false,
  electronDist: 'node_modules/electron/dist',
  mac: { category: 'public.app-category.productivity', target: 'dir', identity: null, notarize: false },
};