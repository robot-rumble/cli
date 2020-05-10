const path = require('path')
const webpack = require('webpack')
const MiniCssExtractPlugin = require('mini-css-extract-plugin')
const HtmlWebpackPlugin = require('html-webpack-plugin')
const CopyWebpackPlugin = require('copy-webpack-plugin')

// NOTE: all NODE_ENV checks must be done in terms of 'production'

module.exports = {
  mode: process.env.NODE_ENV || 'development',
  stats: 'minimal',
  entry: './src/app.js',
  output: {
    path: path.resolve(__dirname, 'dist'),
  },
  module: {
    rules: [
      {
        test: /\.js$/,
        exclude: /node_modules/,
        use: { loader: 'babel-loader' },
      },
      {
        test: /\.(sa|sc|c)ss$/,
        use: [
          {
            loader: MiniCssExtractPlugin.loader,
            options: {
              hmr: process.env.HOT === '1',
            },
          },
          'css-loader',
          'sass-loader',
        ],
      },
      {
        test: /\.elm$/,
        exclude: [/elm-stuff/, /node_modules/],
        use: (process.env.HOT ? ['elm-hot-webpack-loader'] : []).concat([
          {
            loader: 'elm-webpack-loader',
            options: {
              optimize: process.env.NODE_ENV === 'production',
            },
          },
        ]),
      },
      {
        test: /\.(woff|ttf)$/,
        use: [
          {
            loader: 'url-loader',
          },
        ],
      },
      {
        test: /\.raw.*$/,
        use: 'raw-loader',
      },
    ],
  },
  resolve: {
    alias: {
      'battle-viewer': path.resolve(__dirname, '../../battle-viewer'),
    },
  },
  plugins: [
    new HtmlWebpackPlugin({ template: 'src/index.html' }),
    new MiniCssExtractPlugin(),
    new webpack.EnvironmentPlugin({
      NODE_ENV: 'development',
    }),
    new CopyWebpackPlugin([
      {
        from: path.resolve(__dirname, '../../backend/public/images'),
        to: 'images',
      },
    ]),
  ],
  devServer: {
    contentBase: '../public',
    historyApiFallback: true,
    stats: 'minimal',
    host: '0.0.0.0',
  },
  devtool: 'source-map',
}
