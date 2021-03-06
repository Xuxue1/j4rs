/*
 * Copyright 2018 astonbitecode
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 * http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */
package org.astonbitecode.j4rs.value;

import org.astonbitecode.j4rs.api.value.JsonValueImpl;
import org.astonbitecode.j4rs.utils.Dummy;
import org.junit.Test;

public class JsonValueImplTest {

    @Test
    public void fromString() {
        JsonValueImpl jvi = new JsonValueImpl("This is a String");
        String json = jvi.getJson();
        String obj = (String) jvi.getObject();
        assert json.equals("\"This is a String\"");
        assert obj.equals("This is a String");
    }

    @Test
    public void fromNumber() {
        JsonValueImpl jvi = new JsonValueImpl(3.33);
        String json = jvi.getJson();
        double obj = (double) jvi.getObject();
        assert json.equals("3.33");
        assert obj == 3.33;
    }

    @Test
    public void fromObject() {
        JsonValueImpl jvi = new JsonValueImpl(new Dummy(3));
        String json = jvi.getJson();
        Dummy obj = (Dummy) jvi.getObject();
        assert json.equals("{\"i\":3}");
        assert obj.getI() == 3;
    }
}
