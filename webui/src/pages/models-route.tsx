import { useLocation } from "react-router-dom";

import ModelsPage from "@/pages/models";

export default function ModelsRoute() {
  const location = useLocation();
  return <ModelsPage key={location.search} />;
}
