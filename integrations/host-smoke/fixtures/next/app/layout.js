// SPDX-License-Identifier: Apache-2.0

export const metadata = { title: "Next host smoke" };

export default function RootLayout({ children }) {
  return (
    <html lang="en">
      <body>{children}</body>
    </html>
  );
}
