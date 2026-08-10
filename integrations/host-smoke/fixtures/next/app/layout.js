// SPDX-License-Identifier: GPL-3.0-only

export const metadata = { title: "Next host smoke" };

export default function RootLayout({ children }) {
  return (
    <html lang="en">
      <body>{children}</body>
    </html>
  );
}
