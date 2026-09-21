import { Hero, Nav } from "./components/Hero";
import {
  Download,
  Faq,
  Features,
  Footer,
  HowItWorks,
  Positioning,
  Requirements,
} from "./components/Sections";

export default function App() {
  return (
    <>
      {/* 跳到正文。导航里有 4 个链接，键盘用户不该每页都 Tab 一遍。
          平时不可见，获得焦点时才出现（见 styles.css 的 .skip）。 */}
      <a className="skip" href="#main">
        跳到正文
      </a>
      <Nav />
      <main id="main">
        <Hero />
        <Features />
        <HowItWorks />
        <Requirements />
        <Positioning />
        <Faq />
        <Download />
      </main>
      <Footer />
    </>
  );
}
